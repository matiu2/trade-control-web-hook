# BUG — a spread-hour PARK bypasses entry dedup, and its PROMOTION records no attempt

**Status:** OPEN, found 2026-09-18 by the offline replay once the entry-instant
quote made the H4+ park reachable (`feat/replay-upkeep-fixtures`). Live-reachable
on `staging` (park shipped in `feat/spread-gate-park`, 8b142171). Same shape as
the EUR/GBP 3×-risk incident (`BUG-mw-everybar-enter-skips-retry-gate.md`).

## Reproduction (offline, real data)

AUD/NZD D1 iH&S, `--strategy-v2 --qm-entry market`, re-armed from
`~/Downloads/AUD_NZD-TRADENATION-d-20260917T175936.json`, replayed with
`--upkeep 15m` over 2026-08-19T21:00Z..2026-09-29T21:00Z:

- 2026-08-31 21:00Z: qm-market enter fires; the 21:00Z M15 open book is 18.0p >
  7.5p → **PARKED** (SpreadHour). Promoted at the 22:00Z tick → **entered**.
- 2026-09-01 21:00Z: the enter fires again (next transition bar). The spread gate
  runs BEFORE the retry gate, so it **parks again** — the open-position backstop
  never runs. Promoted next hour → **a second position on the same trade**.
- Result: 2 legs, +2.10R, where per-bar (no upkeep) books 1 leg, +1.05R with the
  second fire correctly `rejected: trade-already-open (backstop)`.

## Two holes, both in `core/src/dispatch/enter.rs`

1. **The park sits above the retry gate.** The gate was deliberately moved LAST
   ("never cancel an order you cannot re-place"), and every reject-capable gate
   sits above it. A park is a reject that later turns into a placement, so it
   needs the dedup the gate provides — and gets none. Any H4+ enter that fires
   while a position is open or an order is resting, inside a spread hour, parks
   and is later placed on top. M/W (`FireMode::EveryBar`) on H4 would park
   **every bar** of a spread hour.
2. **`EntryOrigin::Promotion` is `is_replacement() == true`**, so the promotion
   re-drive skips the retry gate AND `record_placement`: no `EntryAttempt` row is
   written. Consequences: (a) no dedup for the promotion itself; (b) the next
   fresh fire's gate sees `attempts == []`, skips the broker backstop by design,
   and places again — even outside a spread hour; (c) every attempt-keyed cron
   (pending sweep, break-even, blackout passes, order-control re-price) is blind
   to the promoted position — the exact "manual entries unmanaged" class (v145).

## Fix direction (not done — needs the operator's eyes, it is live-money dedup)

- Before parking, run the gate's READ-ONLY dedup: an open position or a resting
  order for this trade ⇒ `rejected: trade-already-open`, no park. Do not call
  `retry_gate::evaluate` wholesale above the SL floor — its `Pending` arm cancels.
- A promotion must produce an `EntryAttempt`: either treat `Promotion` as a fresh
  fire for the gate (it has no prior placement to re-point, unlike `Replacing`;
  the `retry-fire-replay` same-bar check must then key on the promotion instant,
  not the original `shell.time`), or write the row explicitly on the promotion
  path with the next attempt number.
- Pin both with entry-point tests at `run_enter` (SpyBroker with an open
  position ⇒ a spread-hour fire on H4 is rejected, not parked; a promotion
  leaves exactly one `EntryAttempt`) and the replay test above.

## Why no fixture caught it

The corpus has no D1 cells with a wide close book, and until the entry-instant
quote existed the offline gate could not trip on aggregated TradeNation D1 at all.
