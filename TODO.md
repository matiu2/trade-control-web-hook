# TODO: auto TP-resistance band — same width as a drawn S/R line

**Rule (operator 2026-07-24):** the auto TP-resistance band is currently HALF the
width of a drawn S/R band for the same `pct` (drawn = `±pct` = 2·pct total; auto
TP = one-sided `+pct` = 1·pct total). Fix: move the band's **center** to the
approach-side offset `TP ± pct` and build a normal `±pct` band around it — so the
band's edge lands exactly on TP (a clean run to TP is unaffected, never extends
PAST TP) but its total width now equals a drawn line's, reaching further up the
approach side to catch a reversal short of TP. Keep the default `pct` unchanged
(operator: this won't fix the XAU_XAG 0.25%-short reversal — that's a separate
width-tune decision).

Geometry (pct as fraction):
- Short (falls into TP from above, reversal ABOVE TP): center `TP·(1+pct)` →
  band `[TP, TP·(1+pct)·(1+pct)]` → edge (lo) = TP, reaches up the approach.
- Long (rises into TP from below, reversal BELOW TP): center `TP·(1-pct)` →
  band `[TP·(1-pct)·(1-pct), TP]` → edge (hi) = TP.

## Steps
- [x] 1. `tv-arm/src/pipeline.rs::tp_resistance_band`: center at approach-offset
      `TP·(1±pct)` + normal ±pct band. Far edge = TP; near edge reaches 2·pct.
- [x] 2. Updated far-edge tests: edge still TP, new near edge asserted.
- [x] 3. New test `tp_resistance_band_matches_a_drawn_sr_line_width` (width ==
      drawn band, ~2× the old one-sided).
- [x] 4. `hs_default_adds_tp_resistance_band` still green (edge still == TP).
- [x] 5. tv-arm 263 tests green; clippy clean; fmt.
      (XAU_XAG short: band 68.324→[68.324, 68.461], was [68.324, 68.392].)
- [ ] 6. CHANGELOG vNN; commit+push; merge staging + redeploy; parent pointer.
- [ ] 7. (still queued, separate) uk100 fixture rebless.
