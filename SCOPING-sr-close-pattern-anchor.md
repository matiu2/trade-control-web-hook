# SCOPING — S/R reversal-close should test a pattern-aware anchor, not the bare close

> **STATUS: IMPLEMENTED (v111, 2026-07-22).** Shipped as a shared pure fn
> `core/src/signals/band_anchor.rs` (no wire field — the shell already carries
> `signal_kind`+`open`, and the direction is derived from body colour worker-side
> / passed from `LatchedSignal` engine-side). Engine `close_windows_pass` and
> worker `run_close` both call it. See CHANGELOG v111. The doc below is the
> original design note; the "bake onto the shell" option was simplified to a
> shared fn since both sides already hold the inputs.

## Motivation (the UK 100 case)

Replay `tv-arm-staging --replay --strategy-v2 --save uk100-qm-v2-confirmation-fixed`
closed a long on `07-close-on-sr-reversal` at `2026-07-17 11:00 +10`
(`2026-07-17T01:00:00Z`). Operator flagged it as wrong: "it didn't come off a
level of support."

Root cause is geometry, not a code fault: the S/R-reversal close currently fires
when the **reversal candle's CLOSE** lands inside a drawn S/R band. The reversal
bar was:

```
o=10551.7  h=10559.7  l=10532.1  c=10532.9     (a bearish engulfer)
```

Drawn S/R line at ~10525.3 → band `[10514.7747, 10535.8253]` (±0.1%
`reversal_band_pct`). The **close** 10532.9 is inside the band, so it fired. But
the bar **opened 16 pts above** the band and **fell into it** — that's price
dropping *into* support (continuation), not *bouncing off* it (reversal). The
designed intent is "a candle that bounced back out of the zone."

## The designed rule (operator's framing)

The band test should key on the part of the candle that represents the
**rejection point** at the level — the point that "merges with the band" when the
candle bounces out of the zone. This is **pattern-aware**:

| SignalKind | anchor tested against the band | rationale |
|---|---|---|
| `RegularEngulfer` | **candle open** (`o`) | opened at/into the level, engulfed back out |
| `FloatingEngulfer` | **candle open** (`o`) | same |
| `Pinbar` | **wick 50%** (midpoint of the rejection wick) | the wick is the rejection; its middle must merge with the band |
| `Tweezer` | **wick 50%** | wick-rejection pattern (relaxed pinbar leg) → same as pinbar |
| `DoubleTweezer` | **wick 50%** | same |

Wick-50% geometry (direction-aware — a reversal-close of a LONG fires on a SHORT
signal, and vice-versa):

- **Short signal** (bearish, upper-wick rejection):
  `anchor = body_top + (high - body_top) / 2`  where `body_top = max(o, c)`
- **Long signal** (bullish, lower-wick rejection):
  `anchor = body_bot - (body_bot - low) / 2`   where `body_bot = min(o, c)`

Decisions locked with the operator:
- **Pinbar → wick 50% (midpoint)**, not the tip and not whole-wick-overlap.
- **Engulfer → raw candle open `o`** (simplest "opened in the zone" reading;
  reproducible from a single baked field on both sides — see below).

Applying this to the UK 100 bar: it's an engulfer → anchor = open = **10551.7**,
which is **outside** `[10514.7747, 10535.8253]` → **would NOT fire.** Matches the
operator's instinct.

## The hard constraint: replay == live (`[[strategy_changes_in_both_replayer_and_worker]]`)

The band check exists in **two** places that must not diverge:

1. **Engine / replay** — `engine/src/evaluate.rs::close_windows_pass` →
   `price_in_any_band(candle.c, &intent.sr_bands)`. Has the full candle + the
   `SignalKind` (via the latched signal). Could compute the anchor locally.
2. **Live worker** — `core/src/dispatch/close.rs::run_close` →
   `price_band_hit(broker.get_current_price(instrument), ranges)`. Tests a
   **single live scalar price** from the broker. It has **no OHLC, no wick, and
   no SignalKind** at this point.

So the pattern-aware anchor is **not expressible in the worker today** — it only
has "current price when the close alert fired." Implementing the rule in the
engine alone would reintroduce a replay↔live divergence (exactly the bug class
the `close_windows_pass` comment already documents for the news-gate AND/OR fix).

## Chosen design: bake the anchor into the signed shell (Option A)

Compute the anchor **once, in the detector**, and bake it onto the signal shell
so **both** the engine and the worker test the *same* value against the bands —
mirroring how `pip_size` / `tick_size` are baked (`[[pip_size_baked_into_intent]]`).

- Detector (`core/src/signals/detect.rs::build`) already knows `SignalKind`,
  direction, and the candle OHLC. Add a `band_anchor: f64` to the signal
  geometry / shell: open for engulfers, wick-50% for pinbar/tweezer/double.
- Signed shell gains a `band_anchor` field (whole-body HMAC, tamper-proof), the
  way `signal_high`/`signal_low`/`signal_kind` already ride the shell.
- **Engine**: `close_windows_pass` tests `price_in_any_band(shell.band_anchor, …)`
  instead of `candle.c`.
- **Worker**: `run_close` tests `price_band_hit(shell.band_anchor, ranges)`
  instead of `broker.get_current_price(...)`. Removes the live-price fetch from
  the S/R gate entirely (the news gate is unaffected).

One value, one source (the detector), tested identically both sides → no drift.

### Why not the alternatives
- **Engine-only change** → replay ≠ live. Rejected.
- **Worker fetches the just-closed candle + reimplements the anchor** (Option B)
  → duplicates the detector geometry in the worker, two drift surfaces. Rejected
  in favour of bake-once.

## Follow-ups / open questions
- `require_price_in_ranges` (the deprecated pre-`sr_bands` form) shares
  `price_band_hit` in the worker. Decide whether the legacy form also switches to
  the baked anchor or stays live-price (it predates `sr_bands`; likely leave it,
  since nothing new emits it).
- `reversal_band_pct` default (0.1%) is unchanged by this — the *width* of the
  band is orthogonal to *which point* we test. Worth a separate look: ±10.5 pts on
  UK100 is generous, but that's a tuning question, not this fix.
- Tweezer/double-tweezer signal span covers 2–3 bars; the anchor is computed on
  the **current** (most recent) bar's wick, consistent with how the pinbar leg is
  the rejection. Confirm with operator if the tweezer anchor should instead be the
  combined wick.

## Test plan
- Detector unit tests: `band_anchor` == open for both engulfer kinds; ==
  wick-50% for pinbar (long & short); tweezer/double follow pinbar.
- Engine test: replay the UK 100 fixture → `07-close-on-sr-reversal` no longer
  fires on the `2026-07-17T01:00:00Z` engulfer (open out of band); a synthetic
  bullish engulfer that *opens* inside a band still fires.
- Parity test: same shell `band_anchor` drives engine and worker to the same
  Passed/Failed on a table of (kind, direction, band) cases.
- Regression: the existing `close_windows_pass` / `price_band_hit` tests keep
  passing for news-gate behaviour and for genuine off-the-level reversals.
