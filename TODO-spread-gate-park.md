# TODO — spread-blackout gate: park (delay) on H4+ instead of reject

Branch: `feat/spread-gate-park`
Worktree: `../trade-control-web-hook-upkeep-ticks` (sibling — path-dep rule)
Follows job 2 (`--upkeep`, merged 7e9b824c).

## Why

`run_enter`'s spread-blackout gate REJECTS a fresh enter whose live spread is
> 5× the baked median inside the NY-close window. Reject = Skip: nothing is
parked, and a once-mode enter refires only on the next signal bar. On a daily
plan that is a day later, so the trade is lost, not delayed. Operator
decision 2026-09-18: H1 and below keep the reject (the setup can change in an
hour); H4 and above DELAY — park as Stored, promote when the spread hour is
over. Market-hours gate stays a reject on every granularity.

Shared code: the park is the existing Stored/promote machinery; the release
rule is the lifecycle's OFF predicate (baked hour ended OR live spread
recovered), extracted into one fn both call.

## Steps

- [x] 1. `stored.rs`: `StoredReason::SpreadHour`; `StoredCheck.spread_hour_over`;
      `stored_verdict` reads it for that reason. `spread_gate_defers(gran)`:
      H4/D1 ⇒ true, else false (None ⇒ false). Tests.
- [x] 2. `spread_blackout.rs`: pure `spread_hour_released_at(instrument, pip_size,
      measured_spread_price, now)`; `pending_lifecycle::spread_hour_released`
      and `tick::promote_due_orders` both call it. `park_order` records
      `pip_size` so early release can convert to pips. Tests.
- [x] 3. `enter.rs`: on a spread-blackout trip with `spread_gate_defers`, park
      with `SpreadHour` (drawn sl/tp distance, min_r) and return the Rejected
      with a "(parked)" outcome, mirroring the BelowMinR park. Test: H4 parks,
      H1 rejects without a park.
- [x] 4. Replay entry-point test (`replay.rs`): an H4 enter firing on the
      spread-hour bar is parked and promoted by the next clean upkeep tick /
      bar, entering at the calm spread. Requires the replay quote clamp to be
      removable — see step 5; until then the replay gate is inert.
- [~] 5. Remove `ReplayBroker::get_quote`'s in-spread-hour clamp (audit
      finding #10) as its OWN commit; measure the corpus delta; re-bless with
      a note.
- [ ] 6. clippy + fmt; commit + push each step.
