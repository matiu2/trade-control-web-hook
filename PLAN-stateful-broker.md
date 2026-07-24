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
