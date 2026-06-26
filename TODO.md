# TODO — tv-arm: filter drawings to visible window before role-matching

## Problem
`tv-arm` role-matching reads ALL drawings on the chart (entire history), not
just those in the visible window. In **live arming** (`SlotPref::LatestWins`)
the window is ignored entirely — a stale off-screen drawing with a newer anchor
time wins the single-slot tiebreak over the correct in-view drawing. Recurs
constantly; hand-fixed by deleting stale drawings. Structural fix: drop
drawings that lie entirely outside the visible window (intersection, not
containment) BEFORE the tiebreak, in BOTH run modes.

Concrete repro: CAD/JPY H1, visible May 15→27 (1778810400..1779843600). Correct
neckline `1kUSW4` (May 18→20) lost to June `2Xfe1I`/`7rdwbe` pair under
LatestWins. Only `1kUSW4` is in-window; window-filtering collapses every role
to one unambiguous drawing.

## Plan
- [x] Read roles.rs / drawings.rs / pipeline.rs — understand current modes
- [x] Add `Drawing::intersects_window(from, to)` (intersection, not
      containment) + `earliest_time()` helper, unit-tested in drawings.rs.
- [x] `pick_slot` window-filters to in-window first (BOTH prefs, real visible
      range threaded independent of SlotPref), logs `in_window=N
      dropped_out_of_window=M`, WARNs on >1 in-window, falls back to full set
      only when nothing is in-window. SlotPref now governs only the tiebreak.
- [x] `pick_trade_expiry`: intersection OR within forward margin (= window
      width) of `to`; prefers expiry nearest the right edge.
- [x] Tests: CAD/JPY repro, intersection edge cases (whole-view span, single
      partly-off-screen kept), expiry forward margin + off-screen-left dropped,
      both modes, in-window newest tiebreak. 35 roles tests, all green.
- [x] cargo test (211 workspace), clippy -D warnings, fmt — all clean.
- [x] README: documented single-slot visible-window scoping.
- [ ] Commit + push main; deploy dev + staging; advance parent pointer.
