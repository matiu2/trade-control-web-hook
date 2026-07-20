# TODO — pcl-exhausted / too-low level = fib 1.8 (neckline-anchored)

## Request

Operator reads the pcl-exhausted abort off the chart at the fib **1.8** level
(fib drawn `head(0) → neckline(1)`, so `2.0` = TP). The code fired ~one notch
too shallow. Make the `too-low` / pcl-exhausted level = the fib 1.8 = 8785.8 on
the AU200_AUD 2026-07-20 setup, WITHOUT adding a new tv-arm param.

## Root cause

`pcl_exhausted_price` (`tv-arm/src/geometry.rs`) used
`midpoint + 0.8·(TP − midpoint)` with `midpoint = (head+neckline)/2` ≈ fib 1.7
(8789.28). The neckline is ALWAYS exactly the fib 0.5 (forced by
`TP = 2·neckline − head`), so no extra input is needed — head/neckline/TP fully
pin the fib.

## Fix (DONE — code + tests green)

- [x] `pcl_exhausted_price` → `neckline + 0.8·(TP − neckline)` = fib 1.8.
      Anchors on the neckline, deeper (closer to TP) than before.
- [x] This matches M/W `overshoot_level` exactly (already
      `neckline + 0.8·(TP − neckline)` ≡ `180% of top→neckline`). H&S + M/W now
      abort at the same fraction — consistency, no new param.
- [x] Tests: `pcl_short` 1.03→1.02, `pcl_long_mirrors_short` 1.17→1.18, new
      `pcl_equals_neckline_plus_80pct_of_neckline_to_tp` (identity + deeper);
      pipeline `hs_entry_level_vetos_short_...` baked veto 1.0830→1.0820.
- [x] tv-arm 241 pass, core 879, engine 166; clippy clean; fmt clean.

## Docs / memory

- [x] README two exact-formula spots updated (`neckline + 0.8·(TP−neckline)` = fib 1.8).
- [x] CHANGELOG v107.
- [x] memory `pcl_exhausted_is_fib_18_neckline_anchored.md` + MEMORY.md pointer.

## Verify (end-to-end)

- [x] Fresh plan bakes too-low = **8785.81** (was 8789.27). ✓ (= operator's 8785.8)
- [ ] Replay: 11:30 bar (OANDA low 8789.0) no longer trips too-low (now 8785.81);
      report where the trade goes. ← in progress (replay107c.txt)

## Ship

- [ ] commit + push staging (tag v107) + parent bump.
- [ ] cherry-pick to main.
- [ ] rebuild suffixed CLIs (-staging + -dev) via deploy (CLI-only; NOT the worker).
