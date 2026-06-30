# Broker-seam parity — ReplayBroker vs the real brokers

**Task #6 part B, the "parity gate".** This document characterises where the
offline `ReplayBroker` (the fake used by `cli/src/bin/replay_candles`) diverges
from the real `broker_oanda::OandaBroker` and
`broker_tradenation_adapter::TradeNationAdapter`, and argues each divergence is
**sound**: the replay is either *faithful* (predicts the same decision the live
worker would make) or *conservative* (never optimistic — never predicts a fill
or a pass the live worker wouldn't give).

## Why the seam is the only thing that can differ

The CF→VM+Postgres migration collapsed dispatch + engine + gates + cron into one
shared implementation (`trade_control_core::dispatch::*`,
`trade_control_engine::*`, `trade_control_cron::*`). The native binary
(`trade-control-worker`), the wasm worker (`trade-control-web-hook`), and the
offline replay all call those *same* functions. So native-vs-wasm decision parity
is guaranteed by construction — there is no second decision implementation to
diff. The StateStore backends are parity-tested separately
(`core/src/state/conformance.rs` runs the same suite against both `MemStateStore`
and `PgStateStore`).

The **only** component that is genuinely substituted between an offline replay
and a live run is the `Broker`. Everything around it is identical shared code.
So the parity question reduces to: **is `ReplayBroker` a sound stand-in for the
real brokers at the 11-method `Broker` trait surface?**

The `Broker` trait (`core/src/broker.rs`) has these methods: `place_entry`,
`close_positions`, `cancel_pending_for_instrument`, `lookup_attempt_state`,
`cancel_order`, `get_quote`, `list_open_positions`, `amend_stop`,
`list_pending_orders`, `get_current_price` (a default over `get_quote`), and
`get_candles`.

---

## Verdict

**The broker seam is SOUND.** No unsound divergence was found. Every place where
`ReplayBroker` differs from the real brokers is either faithful to the states the
shared decision code branches on, or is a method the replay deliberately does not
exercise (so its stub can't change a decision). The two quote-based entry gates
(`spread-blackout`, `SL-spread-floor`) **are now reproduced offline**:
`ReplayBroker::get_quote` synthesizes a two-sided `Quote` from the fire bar's
bid/ask close (`bid_c`/`ask_c`), so a sustained wide spread in the candle data
drives the same 423/422 rejection the live worker makes. The residual gap is now
only the sub-bar timing edge:

> The replay reproduces a spread-blackout / SL-spread-floor rejection whenever
> the **fire bar's** bid/ask close is wide. What it cannot see is a spread that
> **spikes intrabar and retraces by the bar close** — the live worker, sampling
> at the instant of fire, could catch that spike and reject; the replay, reading
> the closed bar's book, would not. This residual is **smallest exactly where the
> gate matters most**: the spread-blackout window targets the sustained
> post-NY-close liquidity trough, where spreads stay wide across whole bars, not
> single-tick spikes. On **TradeNation** the gap is essentially nil — the live
> `get_quote` is *itself* the last 1m bid/ask candle close, so the replay's
> bar-close sample is the same kind of figure, only at the replay candle's
> granularity. On **OANDA** the live quote is a true tick, so the replay's
> bar-close sample is a genuine (but bounded) approximation. See the `get_quote`
> row for the full argument.

No `⚠️ Known unsound divergence` was found (a true unsoundness would be: the
replay predicting a **fill or a pass the live worker would not give**, or
**rejecting something the live worker would fill**, *outside* an already-fail-open
gate). The sub-bar-spike residual is a fidelity ceiling of candle-based replay
(the same ceiling the fill simulator has), not a seam-specific unsoundness; it is
called out so it is not mistaken for full tick parity.

---

## Per-method table

| Broker method | ReplayBroker behavior | Real-broker behavior (OANDA / TradeNation) | Divergence | Sound? Why |
|---|---|---|---|---|
| **place_entry** | Does **not** POST. Consumes an out-of-band "armed" placement (`arm_placement`: intent + shell + the order id to return), records a `PlacedAttempt`, and returns that armed order id. No-arm → `Err(OrderRejected)` (wiring bug, fails loud). | POSTs a stop/limit/market order with attached SL/TP, runs the full risk-gate + sizing path (equity, FX, units), returns the broker order id. OANDA: `order_create_transaction.id`. TN: upstream order id (or `dry-run-<inst>`). | Replay records the attempt instead of placing; no sizing/risk-cap math runs offline. | **Sound.** The retry gate is shared, so its *expectations* are identical regardless of broker. What it needs from `place_entry` is (a) a returned order id that (b) `run_enter` stamps onto the `EntryAttempt` row and (c) a later `lookup_attempt_state` can correlate. ReplayBroker satisfies all three: the armed id is the standard `{intent.id}-{attempt_no}` shape `run_enter` would use, it's recorded keyed by that id, and `lookup_attempt_state` finds it by that id. The risk-cap/sizing math that only the real broker runs is broker-internal sizing, not a *dispatch decision* — the shared gates (RR floor, SL-spread floor) run before `place_entry` in shared code and are identical. The fill itself is modelled separately (see **fills**). |
| **lookup_attempt_state** | Re-simulates the recorded attempt with `simulate_fill` over the candle prefix up to the gate's `as_of` bar, then maps `SimOutcome` → `AttemptState`: `NeverFilled` → `Pending`; `FilledOpen` → `OpenPosition`; `StoppedOut` → `ClosedLossOrBreakeven`; `TookProfit` → `ClosedWin`; `Declined`/`SpreadBlackout`/`Unresolved` → `Cancelled`; `attempt.cancelled` short-circuits to `Cancelled`. Unknown id → `Unknown`. | Queries live broker state via the shared four-step algorithm: pending order → `Pending`; open trade matched by originating order id → `OpenPosition`; closed trade by snapshotted trade id → `ClosedWin`/`ClosedLossOrBreakeven`; else `Cancelled`/`Unknown`. Transient failure → `Err(Transient)`. | Replay derives state from the candle path; real brokers read it from the venue. | **Sound and faithful to the branched-on states.** The retry gate (`core::retry_gate`, shared) branches on exactly the `AttemptState` variants the replay produces. The mapping is faithful: a resting-not-yet-filled order is `Pending` (so a sibling enter cancel-and-replaces it, as live), a filled-still-open order is `OpenPosition` (so a fresh enter is rejected `trade-already-open`, as live), a closed leg is `Closed*` (so re-entry is allowed, as live). The `as_of` bounding makes the open→closed transition *time-accurate* (test `open_then_closed_as_the_asof_bar_advances`). It never returns `Err(Transient)` — so the replay never exercises the gate's "reject this fire, retry next" transient branch, which is the conservative direction (a live transient would *reject*; the replay instead resolves a definite state, never fabricating an extra fill). |
| **fills** (not a trait method; `place_entry` does not fill) | `simulate_fill` (engine) walks the bid/ask candle path separately: a stop/limit fills only once price crosses on the correct book side, **skipping the fire bar** (a resting order isn't live until its bar closes), then SL/TP are detected on the bid (long exit) / ask (short exit). | The venue fills server-side at the live touch, with real slippage, partials, gaps, and weekend handling. | Replay models fills from closed-bar OHLC bid/ask; the venue fills tick-by-tick. | **Sound — deliberate offline stand-in, conservative on the fire bar.** The bid/ask price-path model is the explicit offline substitute for a server-side fill. It is *conservative* about same-bar fills (skips the fire bar; an earlier fix corrected a 1-bar-early fill). It cannot see intrabar tick ordering, so a bar that touches both SL and TP is resolved by the engine's documented ambiguity rule, not by replay choice. This is a known fidelity ceiling of any candle-based simulation, not a seam-specific unsoundness; it is the same model every backtest in this repo uses. |
| **get_quote** | Synthesizes a two-sided `Quote { bid: c.bid_c, ask: c.ask_c }` from the candle at/just-before the `as_of` bar (the fire bar the replay loop pinned via `set_as_of` before dispatching). No candle before `as_of` → `Err(Transient)` (fails open, as live on a quote hiccup). | Returns a live two-sided `Quote`. OANDA: pricing endpoint best bid/ask (true tick). TN: resolve market → `latest_bid_ask` (last 1m bid/ask close). Transient failure → `Err(Transient)`. | Replay reads the fire bar's closed-bar book; live reads a tick (OANDA) or the last-1m-close (TN). | **Sound — reproduces the common case, with a bounded sub-bar residual.** Both quote consumers in `core/src/dispatch/enter.rs` sample `quote.spread()`: the spread-blackout gate (only inside an open blackout window) and the SL-vs-spread floor (every entry). The replay now feeds them the **fire bar's real bid/ask close**, so a sustained wide spread in the candle data drives the *same* 423/422 rejection the live worker makes — the gate is byte-identical and now gets a real spread on both edges. **TradeNation parity is essentially exact** because the live `get_quote` is itself the last 1m bid/ask close, the same kind of figure. **OANDA** samples a true tick, so the replay's bar-close spread is a faithful proxy for sustained-wide windows but **cannot reproduce a spread that spikes intrabar and retraces by the close** — that single sub-bar case is the only residual, and it is the fill simulator's existing closed-bar ceiling, not a new seam gap. Both gates still **fail open** on `Err` (no candle before `as_of`), matching the live fail-open on a quote-endpoint hiccup. `get_current_price` (the default over `get_quote`) now likewise returns the bar mid; its only caller is the too-close fallback, which the replay never reaches (see `place_entry`). |
| **close_positions** | No-op, returns `false` (nothing closed). | Closes all positions for the instrument; returns whether anything closed. OANDA: `close_position`. TN: upstream `close_positions`. | Replay doesn't actually close. | **Sound — not the seam the replay validates.** The replay characterises *entry/re-entry* decisions. Closes/reversals are applied by the engine's report post-pass (`report::apply_reversal_close` / `FillKind::ClosedOnReversal`) over the simulated path, not by a broker round-trip, so the broker's `close_positions` return is not consumed by any replay decision. A live close's effect (position gone) is modelled in the simulated path's exit, not here. |
| **cancel_pending_for_instrument** | No-op, returns `0`. | Cancels all pending orders on the instrument; returns the count. OANDA: list pending, cancel each matching. TN: upstream. | Replay reports zero cancelled. | **Sound — only the M/W-cancel and invalidate paths call it.** In `run_enter` the M/W "validity floor breached" path calls it; the replay's M/W cancel still writes the `mw-cancel` veto (shared) which is what actually blocks the next enter. The return count is logged, not branched on. The per-order supersede cancel the retry gate uses goes through `cancel_order` (below), which the replay *does* honour. |
| **cancel_order** | Marks the matching `PlacedAttempt.cancelled = true`; a later `resolve`/`lookup_attempt_state` then returns `Cancelled` regardless of price path. | Cancels one pending order by id; maps any failure to `CancelError::Transient` ("probably filled, re-lookup"). | Replay sets a flag instead of hitting the venue, and never returns `Transient`. | **Sound and faithful.** This is the retry gate's cancel-and-replace path: when a sibling enter supersedes a still-resting prior order, the gate cancels it, then places the replacement. ReplayBroker's flag makes the superseded attempt resolve to `Cancelled` on the next lookup — exactly the live post-cancel state the gate expects (the order is gone, the slot is free). Test `cancelled_order_resolves_cancelled` pins it. Not returning `Transient` is conservative: a live transient cancel would make the gate re-lookup (and possibly find a fill); the replay instead deterministically treats it cancelled, which can never *create* an extra fill. |
| **list_open_positions** | Synthesises one `OpenPosition` per recorded attempt that `resolve`s to `OpenPosition` by the `as_of` bar, keyed back to its order id. | Lists real open positions/trades. OANDA: open trades. TN: `get_account_details().positions`. | Replay reconstructs positions from simulated attempt state. | **Sound — the Bug #11 backstop, kept consistent with `lookup_attempt_state`.** Both are derived from the same `resolve`, so the gate's two correlation paths (by order id and by snapshotted position id) see a consistent picture. It only reports a position when the simulated attempt is genuinely open by the asking bar, matching what the live broker would list. |
| **amend_stop** | No-op, `Ok(())`. | Moves an open position's (or pending order's) SL to `new_stop`, TP untouched. OANDA: `modify_trade_stops`. TN: `amend_order` (UNVERIFIED upstream — see broker doc). | Replay doesn't amend. | **Sound — break-even is modelled in the simulator, not via the broker.** The cron break-even watch calls `amend_stop` live; offline, the break-even SL move is applied inside `simulate_fill` (the signed `Intent.breakeven` + the pure `core::Breakeven` helper, shared by replay and worker). So the *decision* "move SL to entry at 50%-to-TP" is identical on both edges; only the live broker round-trip is a no-op in replay because the simulator already reflects the moved stop. |
| **list_pending_orders** | Empty `Vec`. | Lists resting entry orders. OANDA: pending orders filtered to entry types. TN: opening orders mapped (skipping malformed). | Replay reports none. | **Sound — not consumed by any replay decision.** The retry gate tracks resting orders through `lookup_attempt_state`/`record_placement`, not through `list_pending_orders`. The live sweep/amend paths that read this list are cron-side broker maintenance the replay doesn't run. |
| **get_candles** | Empty `Vec`. | Fetches closed MID candles in `(since, now]`, ascending, filtered strictly after `since`. OANDA: `candles::get_candles`. TN: count-back-from-`now` then `filter_new_candles`, native TFs only. | Replay returns none. | **Sound — the replay feeds candles directly.** The engine cron fetches its per-tick candle window via `get_candles` live; offline, the replay loop supplies the full bid/ask candle window to `ReplayBroker::new` and drives `simulate_fill`/the engine over it directly, so the broker fetch is never the data source. Returning empty can't change a decision because nothing reads it in replay. |

---

## Notes for a future refactorer

- **`ReplayBroker::get_quote` synthesizes the quote from the fire bar's real
  bid/ask close** (`bid_c`/`ask_c` at the `as_of` bar) — it is *not* a fabricated
  figure, it is the actual book the candle data carries. Keep it reading
  `bid_c`/`ask_c` of the `as_of` candle, not a mid ± a fudge factor: a fabricated
  spread would make the replay reject (or pass) trades the live worker wouldn't,
  which is exactly the false divergence to avoid. Sampling the real closed-bar
  book is faithful for sustained-wide windows (the spread-blackout target) and on
  TradeNation is the same mechanism the live broker uses. The only thing it
  can't see is an intrabar spike that retraces by the close — that is the fill
  simulator's existing candle-resolution ceiling, not something to "fix" with a
  synthetic wider quote.
- **Do not return `LookupError::Transient` from `lookup_attempt_state` in the
  replay.** It would inject the gate's transient-reject branch into an offline
  run that has a definite, derivable state — making re-entry counts diverge from
  live for no fidelity gain.
- **Keep `lookup_attempt_state` and `list_open_positions` derived from the same
  `resolve`.** They are the two halves of the retry gate's correlation; if they
  drift, the gate sees an inconsistent broker.
- Any change to a *decision* must land in shared `core`/`engine`/`cron` (per
  `[[strategy_changes_in_both_replayer_and_worker]]`), never only in a broker
  impl — the brokers are I/O adapters, not decision sites.
