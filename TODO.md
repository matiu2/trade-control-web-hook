# TODO — ReplayBroker: orders are state (PR-1 of 2)

**Goal (operator's model):** the simulation broker holds resting orders with
CONCRETE levels and tests price vs those levels directly on the real bid/ask
book — no mid, no trailing spread, no per-path re-derivation of the SL-vs-spread
floor. Dissolves replay↔live divergence #4 at the root: the floored stop is
computed ONCE at placement (as live does, baking it into `EntryRequest`), the
broker remembers it, and every later question reads that one level.

**PR-1 = "orders are state"** (this branch). **PR-2 = sub-bar zoom-in** for a
candle covering both SL and TP (separate branch; keep pessimistic-stop until
then).

## The root fact
`run_enter` floors `resolved.stop_loss` (10× windowed-mean spread) BEFORE
building `EntryRequest`, so `place_entry` receives the FINAL floored
`req.stop_loss` / `req.take_profit` / `req.entry`. Live's broker places exactly
that. The ReplayBroker currently DISCARDS `req.*` (`record_attempt` stores only
intent/shell) and re-derives the floor in THREE places off `entry_spread_price`
(the trailing spread) — `resolve`, `realize`, and `report::resolve_fire_any` —
which is exactly why #4 exists (`resolve` single-sample vs `realize` windowed).

## Steps
- [x] `PlacedAttempt`: added `placed: Option<PlacedLevels>` (entry/stop_loss/
      take_profit) — the concrete resting-order levels.
- [x] `place_entry`: captures `req.{entry,stop_loss,take_profit}` onto the
      attempt (armed path). The reactivate/re-drive path refreshes stored levels
      from the re-drive `req` (restore re-floors at the restore bar).
- [x] `resolve`: walks the STORED levels via `simulate_fill_resolved` (new engine
      seam) — no floor re-derivation. Unresolvable intent → free slot.
- [x] `realize` / `realized_outcome`: walks the stored levels; retired
      `LedgerGeometry.entry_spread_price`. Break-even + reversal-close stay.
- [x] report: display lines (`placed`, break-even, System-2 widen) read
      `fire.placed_bracket` (read back from the broker) via `breakeven_armed_at_
      resolved` / `widened_stop_at_resolved`. The taken-path R already read
      `fire.realized` (ledger). Retired `Fire.entry_spread_price`.
- [x] System-2 widen now measured off the STORED stop (`widened_stop_at_resolved`).
- [x] Kept the ambiguous-bar pessimistic-stop — PR-2 replaces it with zoom-in.

Engine seam: extracted `simulate_fill_resolved` / `widened_stop_at_resolved` /
`breakeven_armed_at_resolved` (take a pre-resolved bracket, no floor front);
the `_windowed` / floor-front variants stay for the (unchanged) live-mirroring
callers. `apply_entry_spread_floor` stays in `engine` (the resolved-front
variants + the `None`-placed fallback still call it).

## Watch
- The report↔ledger "bit-for-bit" shadow-parity gate: BOTH must move to stored
  levels together or they diverge.
- `entry_spread_price` retirement ripples: `Fire`, `record_order`,
  `LedgerGeometry`, driver (`replay.rs`), and the report. Grep before deleting.
- `apply_entry_spread_floor` / `simulate_fill_windowed` stay in `engine` (still
  used by the LIVE-mirroring bracket-display + break-even annotator?) — check
  callers before removing anything from `engine`.
- SL-floor is LIVE placement behaviour: do NOT stop flooring at placement —
  `run_enter` still floors before `EntryRequest`. We only stop RE-deriving it
  downstream. The stored stop IS the floored stop.

## Verify
- [x] New test `resolve_and_realize_agree_on_the_stored_placed_stop`: a short
      placed with a floored 1.1030 stop; a 1.1025 wick (past the signed 1.1020)
      stops NEITHER `resolve` nor `realize` — both honour the stored stop, so the
      #4 corner can't flip re-entry state. Shadow-parity + fixture tests still green.
- [x] `cargo test` workspace single-threaded (48 bins), clippy clean, fmt.
- [ ] Commit + push; merge staging + main; tag; bump parent gitlink; redeploy
      staging; remove worktree. (Then PR-2: zoom-in.)
