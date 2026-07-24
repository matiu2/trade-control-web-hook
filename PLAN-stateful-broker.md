# Plan — stateful ReplayBroker (kill the two-brain re-simulation)

## Goal

Replace the "re-simulate position state on every query" model with a broker that
**holds** state and mutates it as bars advance — so `close_positions` actually
closes a position (today it's a no-op stub returning `false`), and there is ONE
source of truth the engine queries exactly like the live worker queries the real
broker. This structurally eliminates the two-brain bug class.

## The core design

Broker holds three lists + advances per bar:

```
resting: Vec<HeldOrder>       // placed, not yet triggered
open:    Vec<HeldPosition>    // filled, not yet closed  (carries entry, floored SL/TP, BE-armed, widen state)
closed:  Vec<ClosedTrade>     // the P&L ledger (entry, exit, exit-reason, R)
```

`advance(candle)` — called once per bar by the loop, BEFORE engine dispatch —
runs the SAME per-bar precedence `realize` documents today
(`fill → SL/TP → break-even`), via the SAME engine primitives:

1. Fill any resting order the bar triggers (fire-bar skip A2; spread-hour skip
   D7; sub-bar zoom A3 for the fill bar) → move resting→open.
2. Exit any open position the bar hits SL/TP (STOP=floor A4; break-even A5;
   System-2 widen A6; sub-bar zoom A3) → move open→closed.

Engine dispatch (unchanged call sites) then drives:
- `run_enter` → `place_entry` → push a resting order (caps E1/E2 checked first).
- `run_close` (reversal) → `close_positions` → move matching open→closed at the
  bar close. **This is the fix — no longer a no-op.**
- trade-expiry veto (ClosePositions) → `close_positions` → same path (#3).

Reads become pure state:
- `list_open_positions` → map `open`.
- `lookup_attempt_state` → per-order held state (resting=Pending, open=OpenPosition,
  closed=ClosedWin/Loss, cancelled=Cancelled, unknown=Unknown).
- `list_pending_orders` → map `resting`.
- P&L → read `closed` (no post-loop `realized_outcome` pass).

### Why this answers G2 (the two time-windows) for free
State read mid-loop at bar N reflects only advances ≤ N (bounded, replaces
`as_of`/`window_to_as_of`). The terminal read after the last advance is the
full-window P&L (replaces `realize`'s full-forward walk). Reversal/expiry closes
are applied by dispatch AT their real bar during the loop — so the same-bar (#2)
and blocked-re-entry (#1) bugs cannot form: advance fills entry #2, dispatch
closes it same bar, next bar's backstop sees `open` empty.

## Reversal-close set: no longer pre-collected
Today closes are gathered post-loop and applied in `apply_reversal_close`. With
dispatch driving `close_positions` live, the engine's `run_close` fires the close
at its bar and we act on it immediately. We DROP `apply_reversal_close` and the
`> fill_at` filter entirely — the ordering is handled by "advance (fill+bracket)
runs before dispatch (close) on the same bar," which is the live-faithful order.
(Precedence check: today bracket-on-same-bar wins over a reversal via
`c.at < exit_at`. In the new model, advance() applies SL/TP for the bar first,
so if the bar stopped out, `open` is already empty when run_close asks — bracket
still wins on the same bar. Same-bar-as-FILL reversal now applies, which is the
intended #2 fix. Verified consistent with operator rule.)

## Staging (each stage compiles + its tests pass before the next)

## PROGRESS CHECKPOINT (durable — resume here after any context loss)
- ✅ S0 baseline: fixtures copied to `scratchpad/fixtures-baseline/`; golden test
     `all_fixtures_match_expected` was GREEN before changes.
- ✅ S1 DONE (commit after S0): held types `HeldOrder`/`HeldPosition`/
     `ClosedTrade`/`ExitReason` + `resting`/`open`/`closed` RefCell fields added &
     init'd in `new()`. Compiles green (only expected never-read warnings).
- 40-invariant catalog lives in the task transcript / my notes; the executable
     net is: 6 `shadow_parity_*` tests + `resolve_and_realize_agree_on_the_stored_placed_stop`
     + `open_then_closed_as_the_asof_bar_advances` + the golden fixture test.
- HARD RULE from operator: **do NOT run `--rebless`.** If any fixture test fails,
     STOP and show the operator the failure + proposed expected.json diff for
     manual sign-off. Never auto-bless.
- Files in play: `cli/src/bin/replay_candles/replay_broker.rs` (core),
     `replay.rs` (loop), `report.rs` (P&L readout).
- Worktree: `../trade-control-web-hook-expiry-close`, branch `replay-stateful-broker`.
- ✅ S2 committed (advance() + record_attempt pushes HeldOrder). Currently at
     clean S2 (S3 attempt was reverted — see below).

### S3 IN PROGRESS — shadow-parity assert caught a REAL divergence (good!)
First S3 attempt added: cancel_order/reactivate mirrors onto held `resting`, and
a `debug_assert_held_matches_resim` shadow check in the loop. It FAILED on 3
tests — the net working. Divergences found:
  1. cancelled resting order: re-sim=Cancelled, held=pending → FIXED by mirroring
     the `cancelled` flag in cancel_order + reactivate_matching_cancelled onto the
     held resting order. (Reverted with the rest; must redo.)
  2. cancel-and-replace (`a_new_enter_cancels_a_resting_sibling_order_no_overlap`):
     re-sim=Cancelled (absolute — cancel flag overrides price path), held=open
     (the stop FILLED before/around the cancel). THE CRUX: re-sim treats a cancel
     as retroactively absolute ("this order never counted"), but in reality you
     CANNOT cancel-pending an order that already filled. Need the seed/live loop
     ordering (agent abb2fab155ecdf4d5 analyzing) to know if cancel genuinely
     precedes fill in as_of terms. Likely resolution: the shadow assert is too
     strict — held model is the MORE correct one here; relax the assert to tolerate
     the known-benign cancel-vs-fill difference, OR (if held is wrong) fix advance.
Reverted the whole S3 attempt to clean S2 to redo it cleanly with the ordering
facts in hand. Debug prints all removed.

### ✅ S3 DONE + COMMITTED — shadow-parity GREEN across all 109 tests + fixtures
ROOT CAUSE of the cancel-and-replace divergence: `advance()` was bounding at the
bar-CLOSE `now`, but candle timestamps are bar-OPEN times and a bar's close ==
the NEXT bar's open — so `window_to_as_of(<= now)` pulled the next bar's open
price into the fill test and filled the stop a bar early, BEFORE the cancel
landed. FIX: `advance(up_to)` + `prefix_from_fire(shell, up_to)` bound at the
current bar's OPEN (`candles[i].time`), matching the re-sim's dispatch-time
lookups. Loop sets `set_as_of(bar_open)` for advance+assert, then the existing
`set_as_of(now)` at ~L555 restores the bar-close clock for the lifecycle.
This proves `advance()` reproduces the sim EXACTLY as persisted state.

### ✅ S4 DONE + COMMITTED — retry-gate reads use held state
lookup_attempt_state + list_open_positions read held resting/open/closed (advance
to current as_of first so isolated callers see the progression). held_attempt_state
mirrors resolve's mapping incl {id}-pos + ±1.0 sentinels. All green.

### ✅ S5a DONE + COMMITTED — close_positions really flattens
close_positions removes matching held open→closed at bar close, tagged
Reversal/Expiry (loop set_close_reason before dispatch). cancel_pending_for_instrument
cancels held resting. Loop dispatch gained: Action::Close arm (gated by shared
allow_close_gate::evaluate) + ClosePositions-veto arm (trade-expiry flatten).
Shadow-parity relaxed to SKIP reversal/expiry-closed orders (re-sim can't model
them — held model now MORE correct). Report P&L STILL reads old post-loop
realized_outcome pass, so fixtures unchanged, all 109 green.

### ✅ S5b DONE + COMMITTED — report P&L reads held ledger; EUR/USD FIX LIVE
held_realized_outcome reads held closed/open as the report's P&L source (loop
calls it, replacing the realized_outcome re-sim pass). Added FillKind::ClosedAtExpiry
+ render arm + tally.expiry_closes + EXP: marker + annotate label. VERIFIED on
uk-100 fixture: reversal-close books +0.55R ("CLOSED ON REVERSAL"), expiry-close
books +0.80R ("CLOSED AT EXPIRY") — the operator's original ask. Regression test
`stateful_broker_books_reversal_and_expiry_closes_in_the_report` locks it (golden
snapshot CAN'T — it uses the reversal/expiry-blind simulate_fill path in fixture.rs).
110 tests + clippy green. THE TWO-BRAIN BUG CLASS IS DEAD: both position-state
(backstop) and P&L now flow from the one held ledger.

### NEXT: S6–S8 cleanup + S9 verify + S11 simulator move
- S6: open-positions CAP count (place_entry ~L1224) still uses re-sim `resolve` —
  switch to counting held `open`. (Also risk-cap E1 unchanged, fine.)
- S7: spread-hour lifecycle already works via held mirrors (cancel/reactivate);
  confirm list_pending_orders reads held `resting` (may still be re-sim — check).
- S8: DELETE dead re-sim: old `realize`, `realized_outcome`, `apply_reversal_close`,
  `resolve`, `window_to_as_of`, and RETIRE the shadow-parity assert +
  `debug_assert_held_matches_resim` + `held_class`. Rewrite the 6 shadow_parity_*
  tests + realized_outcome parity tests to assert held-state directly.
- S9: full green; golden fixtures already independent (don't move). NO rebless
  needed unless a snapshot moves — if so, show operator plain-English diffs.
- S11: move simulate_fill* out of engine into replay broker (see above).

### (old) S5b plan — switch report P&L to read held `closed` ledger
Replace the post-loop `realized_outcome` pass (replay.rs ~L590) so the report
reads held `closed` (+ `open` for still-open). THIS surfaces the EUR/USD fix
(entry #2 books reversal R, #3/#4/#5 unblock) AND CHANGES FIXTURES. Need a way to
render an Expiry close (FillKind has ClosedOnReversal but no ClosedAtExpiry).
Agent a42340084ef3f0028 mapping report P&L consumption. After S5b: run fixtures,
DO NOT rebless — collect all changed fixtures, give operator plain-English
one-liners for sign-off.

### (old) S4 note — switch reads to held state
`list_open_positions` + `lookup_attempt_state` read held `open`/`resting`/`closed`
instead of re-sim `resolve`. Must keep shadow-parity assert green AND all
fixtures. The open-positions CAP count (place_entry, ~L1172) also currently uses
`resolve` — decide whether to switch it in S4 or S6. EUR/USD re-entries only
truly unblock after S5 (`close_positions` real removes the reversal-closed
position from `open`).

### Stage list
- [x] **S0. Baseline captured**: fixtures copied to scratchpad, golden test green.
- [ ] **S1. Types + held state.** Add `HeldOrder`/`HeldPosition`/`ClosedTrade`,
      the three `RefCell<Vec<..>>` fields. No behavior change yet; old paths stay.
- [ ] **S2. `advance(candle)`** implementing fill + bracket-exit against held
      state, reusing `resolved_for_sim` + `simulate_fill_resolved_zoom` + zoom for
      a SINGLE bar step (not a full-forward walk). Unit-test it reproduces the
      Pending→Open→Closed progression `open_then_closed_as_the_asof_bar_advances`
      asserts.
- [ ] **S3. Wire the loop** to call `advance()` per bar before dispatch; keep the
      old `resolve`/`realize` alive but assert (debug) they agree with held state
      — a live shadow-parity check during migration.
- [ ] **S4. Reads → held state.** `list_open_positions`, `lookup_attempt_state`,
      `list_pending_orders` read the lists. Backstop now frees the slot on a
      reversal-closed position → EUR/USD #3/#4/#5 unblock. Verify on that plan.
- [ ] **S5. `close_positions` real** — reversal + expiry both flatten held open
      positions. Remove `apply_reversal_close` + the post-loop realize pass;
      report reads P&L from `closed`.
- [ ] **S6. Caps on held state** (E1/E2): count `open` as-of the fire bar.
- [ ] **S7. Spread-hour lifecycle on held state** (D1–D8): cancel/restore flips
      a resting order's state; `get_quote` unchanged (pure bar read).
- [ ] **S8. Delete dead code**: `resolve`, `realize`, `window_to_as_of`,
      `as_of` re-sim scaffolding. Rewrite the 6 shadow-parity tests to assert the
      held-state model (the two-path agreement they tested no longer exists; they
      become "held state == expected progression").
- [ ] **S9. Re-bless fixtures** with `--test-mode --rebless`, DIFF each
      `expected.json` against the scratchpad baseline, confirm every change is an
      intended parity gain (reversal/expiry now booked) not drift. Show the diffs.
- [ ] **S10. clippy + fmt; full `cargo test`; run the EUR/USD replay; confirm
      entry #2 books its reversal exit and re-entries fire.**

## S11 (FOLLOW-UP, after S9 green re-bless) — relocate replay-only simulator out of shared engine
Operator insight (2026-07-24): the fill *simulator* is shared engine code but is
EFFECTIVELY REPLAY-ONLY — verified that the only real callers of `simulate_fill`,
`simulate_fill_windowed`, `simulate_fill_resolved`, `simulate_fill_resolved_zoom`
are `replay_candles/*` (broker/report/fixture). The live worker's
`trade-control-cron/src/breakeven_watch.rs` only MENTIONS `simulate_fill` in a
doc comment; it does NOT call it (it shares `breakeven_armed_at` instead). So the
fill-walker should move OUT of `engine/src/simulator.rs` into the replay broker.
CAVEAT: it must keep calling the GENUINELY-shared engine primitives it relies on
(`Resolved::from_intent`, SL-vs-spread floor, break-even arming, spread-hour
suppression, tick-snapping) — those stay in the engine (live uses them). Move
ONLY the 4 fill-walker fns + sub-bar zoom. Do it as a PURE relocation after S9's
green re-blessed baseline, its own commit, fixtures prove behavior-preserving.
Deferred to avoid two tangled structural changes with no green baseline between.

## Invariant catalog = the regression net
The 40-item checklist (scratchpad, from the audit) maps each preserved behavior
to where it moves in `advance()`/reads. I tick each as it's relocated. The 6
shadow-parity tests + `resolve_and_realize_agree_on_the_stored_placed_stop` +
`open_then_closed_as_the_asof_bar_advances` are the executable net.

## Risk & rollback
Each stage is a commit; if a fixture diff at S9 shows an unintended change I can
bisect to the stage that caused it. The worktree branch is
`replay-stateful-broker`; main is untouched until merged.
```
```
