# TODO — news/blackout windows from calendar, not drawn lines

**Branch:** `news-windows-from-calendar`
**Why:** drawn pause/resume/news vertical lines are pure timestamp carriers.
`plan_calendar_bars_within` already computes windows at real event-minute
precision; we throw it away by drawing bar-snapped lines and reading them back.
That readback loses the real minute (30-past-hour on H1) AND causes the
`--start` straddle bug (`in_visible_window` prunes start/end independently →
"1 start / 2 ends"). Replace draw+readback with a direct calendar→roles feed.

## Design decisions (from user)
- tv-arm calls the calendar itself (reuse `fetch_events_for_range`).
- Fixed hours either side (`--news-before` / `--news-after` flags).
- Fully replace drawn lines (delete the classification + pairing pathway).
- Design intrabar reasoning now (fill-instant gating, not bar-granularity).

## Steps

### 1. Native `Window` type in roles  [ ]
- [ ] Add `pub struct NewsWindow { start: DateTime<Utc>, end: DateTime<Utc> }`
      (name TBD) in roles.rs (or a small module).
- [ ] Change `Roles.blackout_pairs` / `news_pairs` from
      `Vec<(Drawing, Drawing)>` to `Vec<NewsWindow>`.
- [ ] Tests: constructor + ordering.

### 2. Populate windows from calendar directly  [ ]
- [ ] New `calendar_windows()` in pipeline.rs: fetch events +
      `plan_calendar_bars_within`, push each row's pause/news
      start/end into `roles` as `NewsWindow`s. No tv-mcp draws.
- [ ] Replace the `auto_draw_calendar_lines` call site.
- [ ] `--news-before` / `--news-after` flags override timeframe buffers.

### 3. Simplify downstream consumers  [x]
- [x] `build_pause_bundles` / `build_news_bundles` read `w.start()` / `w.end()`
      directly (dropped `anchor_time_seconds` → ISO → parse).
- [x] `drop_past_control_pairs` operates on `NewsWindow.is_past(as_of)`.
      Pair-safe by construction — no split bug.

### 4. Delete the drawn-line pathway  [x]
- [x] Removed BLACKOUT_/NEWS_ classification arms in `classify`.
- [x] Removed `pair_vertical_lines` (kept `TimedAnchor` in `pair_lines.rs`,
      still used by single-slot pickers).
- [x] Removed `in_visible_window` + `draw_pair_lines` + `auto_draw_calendar_lines`.
- [x] Updated roles.rs / pipeline.rs tests that asserted drawn pairs.

### 4b. News scope = [cursor, expiry], not visible area  [x]
- [x] `calendar_scope_range(cursor_unix, expiry_hint)` = `[cursor, expiry]`;
      cursor = `--start` or last loaded bar (`bars_range.to`). Chart scroll no
      longer affects which news counts. Tests added.

### 5. Intrabar / fill-instant gating  → DEFERRED TO PR2

### 6. Wrap up (PR1)  [ ]
- [x] cargo test / clippy / fmt across tv-arm + trading-view (192 + 31 pass).
- [x] Verified end-to-end on live EUR/USD (2 events → 2 blackout + 2 news
      windows, server-readable intent format preserved).
- [ ] Update README + CHANGELOG + tag.
- [ ] Merge to main, advance parent pointer.

### PR1b (next, separate) — retire dead drawn-line generators
Per user: SERVER keeps old-style intents; tv-arm/generate don't PRODUCE them.
- Delete CLI subcommands `CalendarBars` / `BuildPause` / `BuildNews`
  (`trade_control.rs`) + `run_calendar_bars` / `run_build_pause` /
  `run_build_news` / `discover_calendar_bundles` (zero callers).
- Drop always-empty `built_calendar` + `BuiltCalendarBundle` arm of
  `append_control_rules`.
- KEEP: `PauseSpec`/`NewsSpec`/`build_pause_from_spec`/`build_news_from_spec`
  (tv-arm's calendar path uses them) + everything the worker/core enforce.

## Status: PR1 done + verified; writing README/CHANGELOG then commit. PR1b next.
