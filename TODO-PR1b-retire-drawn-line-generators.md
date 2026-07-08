# PR1b — retire the dead drawn-line news/blackout generators — ✅ DONE

**Status:** complete. Landed on `news-windows-from-calendar` (continued off PR1
`155371f`). Changelog entry v69. All DONE-WHEN items below satisfied. Tag +
parent-pointer bump still happen at merge to main.

**Branch:** `news-windows-from-calendar` (continue on it, or cut a fresh branch
off it — decide at start). PR1 commit = `155371f` (v68).

**Context:** PR1 moved news/blackout windows to come from the calendar directly
(tv-arm `calendar_windows` → `NewsWindow`). The old drawn-line-era *generators*
are now dead code. User decision: SERVER (worker/core/engine) MUST keep handling
old-style pause/resume/news intents; tv-arm + `trade-control generate` do NOT
need to PRODUCE the drawn-line style (`trade-control` may still READ it).
See memory [[news_windows_from_calendar_not_drawn]].

## KEEP (do not touch)
- `PauseSpec` / `NewsSpec` / `build_pause_from_spec` / `build_news_from_spec`
  (`cli/src/pause_pattern.rs`, `news_pattern.rs`) — tv-arm's calendar path
  (`build_pause_bundles`/`build_news_bundles`) uses these.
- `plan_calendar_bars_within` / `fetch_events_for_range` / `PlanInputs` /
  `CalendarBarRow` / `CalendarBarPlan` — used by tv-arm `calendar_windows`.
- Everything in worker/core/engine (the enforcement side). Verify they still
  build at the end.

## DELETE (dead generators)
1. **cli `trade_control` binary** (`cli/src/bin/trade_control.rs`):
   - subcommand enum variants `BuildPause(BuildPauseArgs)` (~L125),
     `BuildNews(BuildNewsArgs)` (~L134), `CalendarBars(CalendarBarsArgs)` (~L143)
   - match arms `Cmd::BuildPause` (~L840), `Cmd::BuildNews` (~L841),
     `Cmd::CalendarBars` (~L842-845)
   - fns `run_build_news` (~L887), `run_build_pause` (~L912), structs
     `BuildNewsArgs` (~L740), `BuildPauseArgs` (~L757)
   - trim now-unused imports (`run_calendar_bars`, `CalendarBarsArgs`, and any
     Build*Args-only imports) from the `use trade_control_cli::{...}` at L25/L35.
2. **cli lib** (`cli/src/calendar_bars.rs`): `run_calendar_bars` (~L576) and
   `BuiltCalendarBundle` (~L68) IF nothing else uses them after step 3. Also
   `discover_calendar_bundles` if it exists (only a doc ref seen). Re-check
   `cli/src/lib.rs` re-exports (L25/L28) and drop dead ones.
3. **tv-arm** — drop the always-empty calendar-bundle plumbing:
   - `pipeline.rs`: remove `built_calendar` local (L307-308) + the arg passed to
     `register_trade_plan` (L336) + the `built_calendar: &[..]` param (L1749) +
     the `append_control_rules(.., built_calendar)` arg (L1783).
   - `trade_plan_build.rs`: drop the `calendar_bundles: &[BuiltCalendarBundle]`
     param of `append_control_rules` (L138) and its body use; fix the test at
     L1027-1068 (`control_rules_appended_from_pause_news_and_calendar_bundles`)
     — drop the `cal` bundle (L1040) or rename the test.
   - remove `BuiltCalendarBundle` from the `use trade_control_cli::{...}` (L24).

## GOTCHAS
- `BuiltCalendarBundle` is referenced in a tv-arm test (`trade_plan_build.rs`
  ~L993, L1040) — update the test, don't just delete the type out from under it.
- After deleting, `cargo build`/`clippy` will flag any remaining dead re-exports
  in `cli/src/lib.rs` — chase them.
- Order: do tv-arm step 3 FIRST (drops the last `BuiltCalendarBundle` consumer),
  THEN delete the type in cli, so the compiler guides you.

## DONE-WHEN
- `cargo build -p trade-control-cli -p tv-arm -p trading-view` clean
- `cargo build -p trade-control-worker -p trade-control-core -p trade-control-engine`
  clean (server unaffected)
- `cargo clippy` + `cargo fmt` clean, tests pass
- README: drop/adjust any `build-pause` / `build-news` / `calendar-bars`
  subcommand docs (grep README for those names).
- CHANGELOG: fold into the v68 entry (still unreleased) or add v69.
- Commit + push on the branch. (Tag + parent-pointer bump happen at merge to
  main, not now.)
