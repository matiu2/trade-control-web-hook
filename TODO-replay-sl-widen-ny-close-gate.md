# TODO — replay System-2 widen must gate on the NY-close edge

**Bug:** `BUG-replay-sl-widen-uses-wrong-bar-spread-and-synthetic-candles.md`
(demo-journal). Two of its three claims are misdiagnoses; one real gap remains.

## Findings (verified against code)
- **Defect 1 (wrong bar):** misdiagnosis. System 2 is the *NY-close-edge* widen,
  not the entry floor. It is *supposed* to fire on the NY-close bar. The report's
  "thin 07:00 BNE bar" == 01-Jul 21:00 UTC == the NY close (EDT). Working as
  designed. The entry-time floor (System 1) already uses the fire bar's spread
  (`simulator.rs:194 fire.ask_c - fire.bid_c`).
- **Defect 2 (synthetic spread):** already false. Replay pulls real OANDA
  `price=MBA` bid/ask via `get_candles_range_bid_ask` → widens off real
  `ask_c - bid_c`. Nothing synthetic.
- **REAL gap:** the replay's `widened_stop_at` fires on the *first* post-fill bar
  whose spread crosses the trigger, with **no `is_ny_close_edge` gate**. The live
  cron (`trade-control-cron::blackout_apply::apply_if_ny_close_edge`) only widens
  at the NY-close edge. So the replay can widen on a non-NY-close bar the live
  worker would leave alone. Parity divergence.

## Plan
- [ ] Test-first: add a test where a wide-spread bar sits on a *non*-NY-close hour
      → no widen; and one where it sits on the NY-close hour → widen.
- [ ] Move the two existing `widened_stop_at` widen-bar tests onto 21:00 UTC
      (2026-06-17 is EDT) — they currently use 12:00/13:00 UTC and encode the
      pre-fix "any bar" behaviour.
- [ ] Gate the `widened_stop_at` loop on
      `trade_control_core::ny_clock::is_ny_close_edge(c.time)`.
- [ ] Update the `widened_stop_at` doc comment (mention the NY-close gate,
      mirroring the live cron).
- [ ] `cargo test -p trade-control-engine`, clippy, fmt.
- [ ] Update the bug report (verdict + lesson for the journalling LLM).
- [ ] CHANGELOG entry.
- [ ] Commit + push + advance parent gitlink.
- [ ] Refresh `replay-candles-dev` (suffixed binary is a stale copy —
      `[[replay-candles-dev-stale-binary-trap]]`).
