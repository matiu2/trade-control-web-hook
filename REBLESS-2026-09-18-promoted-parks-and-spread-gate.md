# Re-bless 2026-09-18 — promoted parks are booked; the spread gate is live offline

Companion to commits 8b142171 (core: H4+ spread-gate delay) and a0dcd6da
(replay: real in-hour quote, gate-park recovery, order-control tail). Every
golden re-blessed here moved for one of two reasons. Both are the replay
catching up to what live already does; neither is a strategy change.

## 1. A promoted park's trade was placed but never booked (the big one)

Measured on `aud-cad-h1-2026-07-22-normal-news-off-entry-market` at 8b142171:

```
entry placed  … order=…-enter-1784782800 @ 0.9861      ← 05:00Z enter
entry rejected: sl-widen-below-min-r … stored …        ← 06:00Z enter PARKED
stored-order: PROMOTED … → Ok(entered: order=…-enter-1784786400 @ 0.9861)
Done … REV: 1 | Net R: +0.27                            ← one leg reported
```

The promotion placed a real order (live does the same through the same
`promote_due_orders` → `run_enter`), the held ledger filled it and closed it
on the reversal — and the report showed ONE leg. The 06:00Z fire stayed
`Rejected`, `held_realized_outcome` is only read for `Placed` fires, so the
second position's +0.30R (and its reversal close) vanished from every golden
that ever promoted a `BelowMinR` park. The corpus was systematically
under-reporting the trades production takes.

`adopt_promotions` now re-points the parked fire at the order its promotion
placed: same fixture, REV: 2, Net R +0.57. This is what moved the H1 goldens.

## 2. The spread-blackout gate fires offline (audit 2026-09-13 finding #10)

`ReplayBroker::get_quote` used to pin an in-spread-hour quote to EXACTLY the
reject threshold, so the gate's strict `>` never fired: the replay ENTERED on
every NY-close bar live rejects. The real book now flows through. On H1 the
gate rejects (as live); on H4+ it parks and the promotion enters after the
hour off the calm quote (the operator's 2026-09-18 delay rule). This is what
moved the 32 H4 goldens whose enter fires on the 21:00Z-close bar.

## 3. Order-control now outlives the plan

Live's lifecycle / promote / re-price jobs are global loops; the replay
stopped at `eval.done`. It now runs the same per-bar pass over the remaining
bars, so a once-mode enter's park (plan Done the moment it fires) promotes,
and a resting order is still re-priced / pulled after its plan retires.

## Coverage retired by this re-bless

Any golden whose expected R depended on (1) a promoted park being invisible
or (2) an offline entry inside the spread hour no longer covers that shape —
see `[[rebless_can_silently_retire_coverage]]`. The H4 NY-close entries are
now covered by `h4_enter_on_the_ny_close_bar_is_parked_then_promoted_and_fills`
at the replay entry point; promoted-park booking by the same test's realized
assertions.

Pre-existing and NOT part of this re-bless: `uk-100-news-blackout-rentry-
close-on-reversal` already diverged on clean staging (its expected.json has an
uncommitted edit in the primary checkout).
