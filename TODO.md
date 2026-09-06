# TODO — Stage 1: never cancel an order for a fire that can still be rejected

Incident: 2026-08-07 OANDA 101-011-31142393-003, plan `hs-eur-cad-08ca0693`.
`05-enter` ran `retry_gate::evaluate` FIRST (enter.rs:139); the gate found
prior attempt 2318 `Pending` and cancelled it at the broker (retry_gate.rs:227);
the prep gate then rejected the fire with `prep-order-violated (retest)`.
Order 2318 was destroyed with nothing placed and no restore. Setup forfeited.

The rail already exists in prose — `order_control/reprice.rs` module docs:
**"never cancel an order you cannot re-place"** (body verified BEFORE the
cancel). `pending_lifecycle` follows the same store-first ordering. The retry
gate did not honour it.

## Fix: reorder, so the retry gate is the LAST thing before placement.

- [x] Enumerate every reject-capable gate between `retry_gate::evaluate` and
      `broker.place_entry`
- [x] Tests first (all in `core/src/dispatch/enter.rs` `mod tests`)
- [x] Move the retry gate down to immediately before `place_entry`
- [x] `cargo test -p trade-control-core` (1086 baseline + new)
- [x] Mutation-verify at the `run_enter` entry point
- [x] `cargo clippy -p trade-control-core` + `cargo fmt`
- [x] Commit (do not push)
