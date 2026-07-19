# TODO — replay↔live gate parity (#2 market-hours + #3 spread-blackout)

**Goal:** make `run_enter`'s OWN market-hours and spread-blackout gates fire
offline in the replay, by seeding the state-store windows they read — so the
offline entry decision == the live worker's, and the replay-only proxy
re-derivations can be deleted. (Parity-map memory residual #2 + #3.)

**Principle (user directive):** maximize shared code between replay and live.
Reuse the SAME `is_ny_close_edge`, `set_spread_blackout_window`, TTL constant,
and `windows_from_session` the worker uses — no replay-only re-implementation.

## Shared-code prep
- [x] Promote `NY_CLOSE_WINDOW_MARKER_TTL_SECONDS` (3h) from
      `trade-control-cron/src/blackout_apply.rs` → `core::spread_blackout`
      (mirrors `SAFETY_FORCE_RESTORE_SECONDS`'s "lives in core so replay
      matches live" precedent). Cron re-exports it.

## #2 — market-hours gate offline
- [x] Resolve `blackout_windows` in `replay_candles.rs` BEFORE `replay::run`
      (currently resolved after, only for the report). Pass into `run`.
- [x] In `replay::run`, seed once before the tick loop:
      `store.set_blackout_windows(instrument, &windows, now, ttl)`.
- [x] `run_enter`'s `get_blackout_windows` gate now rejects offline →
      `EnterGateOutcome::Rejected { "rejected: market-blackout" }`, no order
      placed, `realized` None. Report renders via existing `rejected_reason`.

## #3 — spread-blackout gate offline
- [x] In the tick loop, before `dispatch_enter`, seed the window per-bar via the
      SHARED gate: `is_ny_close_edge(now)` →
      `store.set_spread_blackout_window(now, NY_CLOSE_WINDOW_MARKER_TTL_SECONDS)`.
      `run_enter`'s gate then samples `ReplayBroker::get_quote` and rejects.

## Remove now-dead proxies (user chose "remove")
CORRECTION mid-task: only the SPREAD-blackout re-derivation was truly dead. The
market-hours `sweep_reason` blackout branch models the RESTING-ORDER SWEEP
(`sweep.rs::market_blackout_act`) — an order placed OUTSIDE the blackout that a
LATER blackout window catches resting. That's a DISTINCT live mechanism the
seeded entry-gate does NOT cover, so it STAYS.
- [x] `engine/src/simulator.rs`: delete `spread_blackout_reject` +
      `BracketReject::SpreadBlackout` + `SimOutcome::SpreadBlackout`.
      (spread-blackout entry gate now fires offline via the seed.)
- [~] `sweep_reason`: KEEP the `market_blackout_due` branch + `blackout_windows`
      param — it's the resting-order-sweep model, NOT the entry gate. Not dead.
- [x] `FillKind::SpreadBlackout` + `FillOutcome::SpreadBlackout`: removed the now
      unconstructable rendering/serialization variants across
      report/annotate/fixture/replay_broker. (Only 1 saved fixture; doesn't use
      it.) A spread-blackout rejection now renders via the `rejected_reason`
      (`GateBlocked`) path.

## Latent LIVE bug surfaced + fixed (restore vs the reject gates)
Seeding the spread-blackout window exposed that the cancel→RESTORE re-drive
(`run_enter(.., restore=true)`) bypasses the retry gate but NOT the two blackout
reject gates — so a restore landing while the ~3h window is open was
`rejected: spread-blackout` and the order DROPPED, never re-placed. Fires in LIVE
too (shared code).
- [x] `core/dispatch/enter.rs`: wrap BOTH blackout reject gates (market-hours +
      spread-blackout) in `if !restore { … }`, same discipline as the existing
      retry-gate bypass. SL-vs-spread floor (a hard per-entry limit) stays.
      Guarded by the existing multishot cancel→restore replay test (fails without
      the bypass now that the window is seeded).

## Verify
- [x] New tests: `enter_inside_market_hours_blackout_is_rejected_by_the_seeded_gate`
      + `enter_inside_spread_blackout_is_rejected_by_the_seeded_gate` (gate, not
      sweep/proxy). Multishot restore test guards the restore-bypass.
- [x] `cargo test` workspace single-threaded (48 bins green), clippy clean, fmt.
- [ ] Commit + push; merge staging + main; tag; bump parent gitlink;
      redeploy staging; remove worktree.
