# TODO — `--sl-anchor` matrix axis

Test the claim that a tighter stop is more profitable than a structural one, by
arming the **same setup** with the stop at three different levels.

The current default is already the *tight* stop: `PriceAnchor::SignalHigh` /
`SignalLow` — the latched **signal candle's own wick** + 0.5%·ATR, resolved live
(`cli/src/trade_patterns.rs:220-240`). It has no connection to the drawn pattern.
So the open question is whether that tight default is being noise-clipped, not
whether we should go tighter.

## The three anchors

| value | price | source |
|---|---|---|
| `signal` (default) | latched signal wick ± 0.5%·ATR | `PriceAnchor::SignalHigh/Low`, live |
| `invalidation` | the drawn `too-high`/`too-low` line | `PlanGeometry.invalidation` |
| `fib-top` | the fib's head (pattern extreme) | `PlanGeometry.fib_head_neckline.0` |

`invalidation` and `fib-top` bake an absolute price at arm time and reuse the
existing `PriceRef::AbsoluteBuffered` path — the same one the `sl` chart Note
takes. No new intent shape, no worker/engine/wire change.

⚠ **Resolve by ROLE, not by literal name.** `too-high`/`too-low` swap roles with
direction (see CLAUDE.md). `geom.invalidation` is already the direction-correct
*drawn* line; the opposite name is the computed pcl-exhausted fib, which sits on
the **profit** side. Anchoring a stop there would place it past the TP.

## Spread: reuse the existing floor, add nothing

`widen_sl_to_spread_floor` (`core/src/intent/sl_spread_floor.rs:164`) acts on the
**resolved** SL price, downstream of *how* that price was chosen, and is already
called on both sides — worker (`core/src/dispatch/enter.rs:740`) and replay
(`fill_sim.rs:937` `apply_entry_spread_floor`). So the new anchors inherit it for
free, and replay↔live parity is structural. **No new spread code in this change.**

Deliberately NOT touched here:

- The `±½ spread` mid→bid/ask correction exists for **M/W only**
  (`core/src/intent/mw_resolution.rs:14`). The H&S drawn-price path (the `sl`
  Note, and now these anchors) doesn't do it. That's a real inconsistency, but
  fixing it would shift every existing `sl`-Note fixture by half a spread — a
  deliberate re-bless, not a side effect of this change. Logged, not fixed.
- Spread *prediction* from hardcoded forward references — out of scope.

## Steps

- [x] Read the SL decision path end to end
- [x] Confirm both levels are plain `f64` in scope at `hs_resolve.rs:310`
- [x] Confirm fixtures carry `invalidation` + `fib_head_neckline` (`.spec.json`)
- [x] Confirm the spread floor is anchor-agnostic ⇒ nothing to add
- [x] `SlAnchor` enum + `--sl-anchor` flag (`tv-arm/src/sl_anchor.rs`, `args.rs`)
- [x] Resolve to a price in `hs_resolve.rs`; route through `sl_on_protective_side`
- [x] Missing geometry ⇒ declined cell (loud), never a silent fallback to `signal`
- [x] Matrix axis behind `--sl-matrix` (default stays 8 cells)
- [x] Corpus re-run utility over the `.spec.json` files
      (`scripts/sl-anchor-sweep.sh`)
- [x] clippy + fmt + 410 tv-arm tests green
- [x] **Mutation-verified** (green tests prove nothing until they can fail):
      fib-top reading `.1` not `.0` → 2 red; protective-side guard removed →
      2 red; missing level falling back to `signal` → 2 red; default cells
      always suffixed (corpus-orphaning) → 5 red. All restored.
- [x] End-to-end: 24/24 cells armed on AUD/CAD 2026-07-22; SL confirmed
      distinct per anchor (`signal_high` anchored / `0.98862` / `0.98882`)
- [ ] Commit, push, merge to main + staging, deploy both, advance parent pointer

## Verified behaviour on the first real setup

AUD/CAD 2026-07-22, `normal-news-on`. The `signal` cell fires `05-enter`
**twice** (05:00 then 06:00 — a re-entry after the first was stopped out); both
structural cells enter **once** and hold. That is the tight-vs-structural
difference showing up as behaviour, which is exactly what the axis is for.

Also visible: `invalidation` (0.98862) and `fib-top` (0.98882) are only ~2 pips
apart on this setup. If that holds corpus-wide they are near-duplicate columns
and the third anchor is not worth its replay time — worth checking on the sweep
output before running all 26.

## Known unrelated failure

`all_fixtures_match_expected` fails **in a worktree** because most fixture cells
are untracked and are never copied in (`?? replay-fixtures/hs-nzd-chf-…`). It
passes in the main checkout. Not caused by this change — verified both ways.

## Corpus re-run — coverage caveat

**26 `.spec.json` files vs 206 fixture dirs.** Only setups armed with
`--spec-out` can be re-armed without a chart. The tool must report the covered
count plainly; implying full-corpus coverage would be the same class of error as
`--rebless` silently covering 19 of 63.

## Out of scope

- `--sl-from-recent` (`args.rs:229`) appears **dead** on the H&S path —
  `sl_anchor` is hard-coded `None` at `hs_resolve.rs:303`, so nothing reads it.
  Noted, not fixed here.
