# PR-2: sub-bar zoom-in for ambiguous SL/TP bars

## Goal
When a single candle's range covers **both** SL and TP, the sim today
pessimistically assumes the STOP (`(true, _) => StoppedOut` in
`engine/src/simulator.rs::simulate_fill_resolved`). PR-2 replaces that, on the
ambiguous bar only, with a **sub-bar zoom-in**: replay finer-granularity candles
for that bar's window and let the first level actually touched decide the outcome.
Falls back to today's pessimistic stop when no finer data is available
(behaviour-preserving for every current caller + fixture).

## Design
- **Data source**: the driver pre-fetches a finer-granularity bid/ask window
  (once) via the same `candles::pull` path it already uses, over the same span,
  and hands it to the `ReplayBroker`. No live fetch inside the pure sync sim.
- **Engine seam**: `simulate_fill_resolved` stays pure/sync. A new `SubBars`
  provider (trait) is consulted ONLY on the ambiguous `(true,true)` bar. `NoZoom`
  (default) → pessimistic stop. The current public `simulate_fill_resolved`
  delegates with `NoZoom` so all callers are unchanged.
- **Zoom rule**: replay the finer bars in `[bar.time, bar.time + bar_len)` in
  order; first that hits SL → StoppedOut, first that hits TP → TookProfit; a
  finer bar itself still ambiguous → pessimistic stop (finest grain we have).
  BE / widen effective-stop is computed once for the parent bar and used for all
  its sub-bars (BE arms on a CLOSE, so it can only change the NEXT parent bar).

## Steps
- [x] engine: `SubBars` trait + `NoZoom`; `simulate_fill_resolved_zoom` variant;
      zoom-aware exit loop (`zoom_ambiguous_bar` + `infer_bar_len`);
      `simulate_fill_resolved` delegates with `NoZoom`.
- [x] engine: 5 unit tests (no-zoom pessimistic; zoom TP-first; zoom SL-first;
      sub-bar-ambiguous → stop; no-covering-sub-bars → stop).
- [x] ReplayBroker: `with_sub_bars` + `FinerSeries` (impl `SubBars`); `resolve` /
      `realize` call `_zoom` with `self.zoom()`.
- [x] driver: `granularity::finer`; pull finer series over the coarse span;
      thread `finer_candles` into `replay::run` → broker. Fail-soft everywhere.
- [x] all `run()` call sites (+fixture) get the extra `&[]` arg.
- [x] cargo test workspace single-threaded — all green incl. all_fixtures_match;
      clippy clean; fmt clean.
- [x] CHANGELOG v101; README replay-sim note.
- [ ] memory update; commit/push/merge staging+main/tag v101/parent-bump/redeploy.

## Watch
- Keep `simulate_fill_resolved` sync & pure — the zoom provider is pre-fetched
  data, not an async fetch threaded through the engine.
- All 16 `ReplayBroker::new` call sites must keep compiling → make zoom additive
  (`new` = no zoom; a `with_sub_bars` builder for the driver only).
- Fixtures re-simulate via `simulate_fill` (no zoom) → outcomes unchanged.
- The finest grain we can pull still has ambiguous bars (a 1-min bar can span
  both). Zoom REDUCES ambiguity, never eliminates it → pessimistic stop remains
  the floor. Log/annotate when we fall back so it's not silently pessimistic.
