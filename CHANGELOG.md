# Changelog

## v75 — 2026-07-09 — --qm-entry: per-leg entry order type for strategy-v2

**Why.** `--strategy-v2` arms two legs (BCR stop `05-enter` + QM confirmed
`09-enter-qm`), each with a fixed order shape. A single `--entry-*` flag is
ambiguous — which leg? — so `--strategy-v2 --entry-stop` was a hard clap
conflict. The operator wanted the **QM leg as a limit** while the **BCR leg
stays a stop** on one setup, which the fixed QM shape (hardcoded `EntryMode::Stop`)
couldn't express.

**What changed.**
- **`tv-arm --qm-entry <market|stop|limit>`** (requires `--strategy-v2`) sets the
  QM leg's order type independently of the BCR leg (`--entry-*`). Default `stop`
  keeps today's shape byte-identical.
- **`TradeSpec.qm_entry_mode`** (`cli`, serde-additive, `skip_serializing_if`
  default Stop). The QM-leg builder reads it instead of hardcoding `EntryMode::Stop`,
  and derives the wrong-side recovery from it: Stop→`recover:limit` (today), Limit→
  `recover:stop` (the v74 rule — a wrong-side limit becomes a stop), Market→`skip`.
- Rejected the alternative `--and`/`--or` connective syntax — a large ambiguous
  argv-expression parser for what is really per-leg control.

**Config.** `tv-arm`: `--qm-entry <market|stop|limit>` (new). `--recover-entry`
gained `stop` in v74.

**Compatibility.** Default (`--qm-entry` omitted) = QM leg stop with limit
recovery, unchanged. `--strategy-v2 --qm-entry limit` → BCR stop, QM limit
(recover→stop).

**Tests.** cli `strategy_v2_qm_entry_limit_makes_qm_a_limit_with_stop_recovery`
(BCR stays stop, QM becomes limit+recover:stop, specs now differ); tv-arm
`qm_entry_flag_resolves_and_requires_strategy_v2`. Existing
`qm_enter_matches_standalone_quasimodo` still green (default path unchanged).
Suites: cli 258 / tv-arm 212 / core 810 / engine 128.

## v74 — 2026-07-09 — first-confirmed entry + pattern entry-order flags + wrong-side limit→stop

**Why.** A confirmed-candle enter (`09-enter-qm`, strategy-v2) never fired on
setups the chart clearly showed confirming. On DE30_EUR H1 2026-07-07 the
operator watched a short confirm at 9pm, but the engine entered nothing and the
plan died on `too-low` at midnight. Three separate causes, all found by replaying
the plan off the candle tape (not the report):

1. **Single-latch confirmation.** The engine's `PinePattern` enter read
   `latched_signal_at`, which keeps one latch slot holding the *most-recent*
   signal. A fresh same-direction print overwrites it, so when an earlier signal
   validates a few bars later its confirmation is written to the latch **only if
   that signal is still latched** (Pine `signal_high_v == sig_high`). In a fast
   run of back-to-back signals the earlier one is displaced before its confirm
   window closes — the confirmation is computed per-signal (chart triangle) but
   never reaches the confirmation-gated enter. **Not a Pine⇄Rust divergence**:
   Pine has the identical guard; it's a single-latch design, and the alerts it
   was built for are retired for this consumer.
2. **QM enter required golden AND confirmed.** The strategy-v2 QM leg inherited
   `spec.needs_golden` (default true), so it needed a signal that was *both*
   golden and confirmed. The DE30 confirmed short (9pm) was non-golden → never
   fired. The operator's confirmation rule has no size test; golden is the
   *break-and-close* leg's gate.
3. **Wrong-side limit dropped.** `EntrySpec::Limit` had no wrong-side recovery
   (only `Stop` did), so a limit at the base — when price had already crossed it
   during the confirmation wait — was dropped as `InvalidGeometry`.

**What changed.**
- **`core::signals::first_confirmed_signal_at`** — replays the same per-bar state
  machine but returns the **earliest-printing** signal (matching the plan's
  direction/kind, at/after a setup floor) to reach `Valid`, carrying *its own*
  base as the entry level. The engine's enter path uses it for `needs_confirmed`
  enters; golden-only enters and the close-on-reversal guard keep the single latch
  (they want "the newest signal now"). Scoped to the setup via the later of
  `break_close_at` and `replay_start` so a warmup-era signal can't claim it.
  `Tracked` gained kind/range/start_time/atr. **Pine unchanged** (alert-only,
  visual confirmation already correct).
- **QM leg drops `needs_golden`** (`cli::trade_patterns`) — confirmation is its
  gate; the BCR `05-enter` keeps golden.
- **`--entry-market` / `--entry-stop` / `--entry-limit`** (`tv-arm`) — expose the
  pattern-path entry order type (was market-or-stop only). Mutually exclusive;
  `--strategy-v2` conflicts with all three.
- **Wrong-side limit → stop** — new `RecoverEntryAction::Stop` +
  `RecoverEntryPlan::Stop`; `EntrySpec::Limit` gains a signed `recover_entry`
  field. The resolver converts a wrong-side limit (recover=Stop) to a stop at the
  same level, re-checked for correct side and re-run through the ≥1R / in-range
  floor — the mirror of the existing stop→limit recovery. `--entry-limit`
  defaults its recovery to `stop` (`--recover-entry` now accepts `stop`).

**Breaking.** `EntrySpec::Limit` gains `recover_entry` (additive,
`skip_serializing_if None` — old plans deserialize with it absent = today's
drop). `RecoverEntryAction` / `RecoverEntryPlan` gain a `Stop` variant.

**Config.** `tv-arm`: `--entry-stop`, `--entry-limit` (new; `--entry-market`
existed); `--recover-entry stop` (new value).

**Tests.** core `first_confirmed_*` (bug-characterisation, direction filter,
not-before floor, fire-once timing), `wrong_side_short_limit_recovers_to_stop`
(+ no-recovery drop); engine `confirmed_enter_fires_off_first_confirmed_signal_base`
(+ pre-confirmation decline); tv-arm `pattern_entry_flags_*`,
`entry_limit_defaults_recover_to_stop`. Verified end-to-end: the DE30 plan now
fires `09-enter-qm` short @ 25705.2 → break-even → +5.26R.

**Follow-up.** First-confirmed is `needs_confirmed`-scoped; a golden-only enter
that overlaps signals still uses the latch (was never confirmation-blocked).

## v73 — 2026-07-09 — invalidation vetos arm pre-break (too-high/too-low fire in AwaitBreakAndClose)

**Why.** A `too-high` / `too-low` invalidation veto is a *setup* invalidation:
price running up away from a short (or down away from a long) kills the H&S/IH&S
setup **before** it ever breaks-and-closes. The engine armed these guards
`AwaitEntry`-only, so a breach while the spine still sat in `AwaitBreakAndClose`
never fired — the plan ran on and was journaled as blocked by a *later*, different
veto. Live and replay agreed with each other but both disagreed with the price
tape. Caught on AUD/NZD H&S short 2026-07-07 (H1, OANDA): price closed above the
1.22001 cap from 07-07 19:00 +10 for hours, yet the recorded blocker was a too-low
at 07-08 13:00 (0R). (`BUG-replay-skips-pre-pause-bars-too-high-never-fires.md`.)

**What changed.**
- **`engine::evaluate::armed_in` → `armed_in_rule`.** Arming now splits by what the
  guard *means*, mirroring `guard_is_terminal`: a **terminal** guard (invalidation /
  cancel veto — `too-high`, `too-low`, the M/W cancel/abort/overshoot vetos — or
  `trade-expiry`) is armed in **every** phase, pre-break included, because a setup
  can be invalidated before break-and-close. A **`Close`** guard
  (`06-close-on-reversal`) is a per-*trade* exit that needs a position, so it stays
  armed `AwaitEntry`-onward. Previously only `trade-expiry` was special-cased to
  all-phases and everything else was `AwaitEntry`-only.

**Behaviour.** On the repro plan the corrected outcome is **too-high @ 07-07 19:00**
(protective — price ran up away from the short), retiring the plan in one fire,
versus the pre-fix **too-low @ 07-08 13:00**. Single fix in the shared `engine`
crate, so live worker (`trade-control-cron`) and offline `replay-candles` both get
it from one place.

**Breaking.** None. No wire/plan/CLI change; internal fn rename only. M/W plans
start in `AwaitEntry` already, so widening the mw-* vetos' arming is a no-op for
them.

**Tests.** `too_high_invalidation_fires_during_await_break_and_close` (the
regression: too-high fires while genuinely pre-break, with a real break-and-close
neckline the up-closing bars never cross down through) and
`close_on_reversal_guard_stays_await_entry_only` (the Close guard is *not* dragged
along — still AwaitEntry-only). Full engine suite green (127).

## v72 — 2026-07-08 — news markers draw by default; `--skip-calendar-bars` skips both

**Why.** v71 shipped the armed-news markers behind an opt-in `--draw-news-markers`
flag. In practice the operator always wants to *see* the armed news when arming a
trade, so the flag was pure friction. Make drawing the default and fold the opt-out
into the existing `--skip-calendar-bars` (which already skipped the windows) so a
single flag turns off the whole calendar step — windows **and** markers — matching
the mental model "no calendar → nothing calendar-related on the chart".

**What changed.**
- **Markers now draw by default.** tv-arm draws the cosmetic armed-news lines
  whenever it arms calendar windows (same one-for-one armed set, same
  `<CCY>-<n>-star-<HH:MM>` Brisbane labels, same same-bar merge). No flag needed.
- **`--draw-news-markers` removed.** The behaviour is unconditional (post-prune,
  so drawn == armed). Nothing to opt into.
- **`--skip-calendar-bars` now skips both** the news/blackout windows *and* the
  markers. It already gated the calendar step, so with markers built from that same
  step, skipping it leaves `news_markers` empty and nothing draws — no separate
  guard needed.

**Breaking.** CLI: `--draw-news-markers` no longer exists (was a no-op default-off
bool in v71; removing it errors on unknown-flag if any script passed it). No
wire/plan change.

**Config.** One fewer flag. `--skip-calendar-bars` semantics broadened to cover the
markers.

**Tests.** Existing `news_marker.rs` + `pipeline.rs` lock-step-prune coverage is
unchanged and still passes; the `defaults_are_sensible` args test still asserts
`--skip-calendar-bars` defaults off.

## v71 — 2026-07-08 — `tv-arm --draw-news-markers` (cosmetic armed-news lines)

**Why.** Since v68/PR1 the news/blackout windows tv-arm reacts to go straight into
the signed plan as `TimeReached` control rules — nothing is drawn on the chart. That's
correct for the gate machinery, but it leaves no visual cue when debugging or replaying
a trade: you can't *see* which events tv-arm armed against. Running `tv-news` standalone
draws a different (looser Medium+, visible-window) set, so its lines don't match what
tv-arm actually watches.

**What changed.**
- **New opt-in flag `tv-arm --draw-news-markers`.** Draws one cosmetic vertical line per
  news event tv-arm reacts to — exactly the High-impact, `[cursor, trade-expiry]`-scoped
  set baked into the plan, drawn *after* elapsed windows are pruned, so drawn == armed,
  one-for-one. Off by default.
- **Label format** is tv-arm's own: `<CCY>-<n>-star-<HH:MM>` in **Brisbane** time (UTC+10,
  matching the operator's charts/journals), e.g. `USD-3-star-04:00`. Events sharing a chart
  bar collapse to one line whose label space-joins each event
  (`USD-3-star-22:00 EUR-2-star-22:30`), since TradingView renders one drawing per bar cell.
- **Cosmetic only.** The lines never touch the signed plan. A tv-mcp draw failure warns and
  continues (deliberately unlike `tv-news`, which hard-errors) so a flaky draw can't block a
  live `--register-plan`.
- **Plumbing.** `calendar_windows` now also returns a `Vec<NewsMarker>` (currency + stars +
  event minute) built from the kept `CalendarBarRow`s — the detail the collapsed `NewsWindow`s
  drop. Stored on `Roles.news_markers`, pruned in lock-step with the news windows in
  `drop_past_control_pairs` (a marker survives iff its post-release news window is still open,
  so a passed-minute-but-open-tail event still draws). New module `tv-arm/src/news_marker.rs`.

**Breaking.** Internal only: `calendar_windows` returns a 3-tuple
`(blackout, news, markers)` (was 2-tuple); `Roles` gains `news_markers: Vec<NewsMarker>`.
No wire/plan/CLI-output change.

**Config.** New flag `--draw-news-markers` (bool, default off). No env, no signed-field change.

**Tests.** `news_marker.rs`: label format (Brisbane time, currency upper-cased, star mapping),
same-bar merge / multi-bar grouping, empty + non-positive-`bar_secs` guards, `resolution_to_secs`
table. `pipeline.rs`: markers pruned in lock-step with surviving news windows (incl. the
passed-event-minute-but-open-tail case).

**Follow-up.** `--skip-calendar-bars` is now misleadingly named (it skips building news windows
*into the plan*, not drawing bars) — an optional rename to `--skip-news-windows` (old name kept
as a hidden alias) is noted but not done here.

## v70 — 2026-07-08 — sub-bar wall-clock news/blackout gating (PR2)

**Why.** Pause/resume/news-start/news-end are wall-clock window edges, and
`tv-arm` bakes the real event minute onto each `TimeReached` epoch (v68). But the
engine tested that epoch per **closed candle** (`candle.time >= at_epoch`), so a
14:30 event on an H1 chart only opened when the 15:00 bar closed — ~30 min late,
and the resume/close was late too. The baked precision was thrown away at the
enforcement layer.

**What changed.**
- **Engine.** Control `TimeReached` rules now fire against the tick's wall-clock
  `now` (already a param of `evaluate_plan` both drivers pass), via a new
  `control_rule_fires` helper. Guards and the spine are untouched — `trade-expiry`
  etc. still fire on `candle.time`. A control rule with a non-`TimeReached`
  trigger (none today) falls back to the candle path, keeping the helper total.
- **Engine.** New `evaluate_controls_only(plan, prior, last_candle, now,
  expires_at)`: runs ONLY the control rules against a wall-clock instant with no
  new candle. Never advances the spine/watermark, never retires the plan. This is
  the shared sub-bar tick entry point both drivers call.
- **Worker.** The 5s engine cron used to bail the instant no new bar had closed;
  it now runs `tick_controls_only` on that path, evaluating the control rules
  against `now` + the last-known bar and dispatching any window edge through the
  same state/dispatch/tick-bundle pipeline. So a mid-bar window opens ~14:30:05
  live, not at the next bar close. No control fire → the old cheap no-op.
- **Replay.** The offline replay owns a virtual clock, so it reproduces the
  worker's sub-bar behaviour **exactly** (not approximately): before each bar's
  `evaluate_plan`, `inject_control_ticks` replays every unfired control epoch in
  `(prev_close, this_close)` by pinning the clock to the epoch and running the
  same `evaluate_controls_only`. Worker and replay open/close every window at the
  identical instant.

**Parity.** The engine boundary predicate is the single source of truth; the two
drivers differ only in *how* they present `now` (worker: real 5s ticks; replay:
injected virtual ticks). New tests assert the worker path and the replay path
fire the same control rule at the same sub-bar epoch, plus an end-to-end replay
where a sub-bar pause suppresses a later enter.

**Breaking.** `evaluate_plan`'s `now` param is now load-bearing for control rules
(was `_now`). Engine gains a public `evaluate_controls_only`.

**Tests.** Engine: `control_window_fires_sub_bar_when_now_reaches_epoch_before_candle_time`,
`evaluate_controls_only_opens_and_closes_a_window_with_no_new_bar`, rewrote the
pause/resume staggering test to drive `now` per tick (added `run_at`). Replay:
`virtual_tick_opens_window_at_same_instant_as_worker_controls_only`,
`sub_bar_pause_epoch_opens_via_virtual_tick_and_suppresses_enter`. 122 engine +
75 replay + 21 cron tests pass; clippy/fmt clean.

**Follow-up.** None outstanding for news gating. The pre-existing
`plan_calendar_bars_within` gap (a USD-high "blocks all instruments" event not
kept for a non-USD pair) is still separate and unaddressed.

## v69 — 2026-07-08 — retire the dead drawn-line news/blackout generators (PR1b)

**Why.** v68 made tv-arm resolve news/blackout windows straight from the
calendar, leaving the old drawn-line generator path dead but still compiled in.
This is the promised PR1b cleanup: delete the code nothing calls anymore.

**What changed.** Removed the `calendar-bars`, `build-pause`, and `build-news`
CLI subcommands and their `run_calendar_bars` / `run_build_pause` /
`run_build_news` handlers, arg structs, and `CalendarBarsArgs` /
`CalendarBrokerArg` / `BuiltCalendarBundle` types plus the `print_summary_table`
/ `row_is_stale` helpers only they used. tv-arm's `register_trade_plan` and
`append_control_rules` drop the always-empty `built_calendar` /
`BuiltCalendarBundle` parameter — control rules now come solely from the
calendar-sourced pause/news bundles.

**Kept (still live).** `PauseSpec` / `NewsSpec` / `build_pause_from_spec` /
`build_news_from_spec` / `write_pause` / `write_news` (the enter-builder and
calendar path still build+sign windows through them), and
`plan_calendar_bars` / `plan_calendar_bars_within` / `fetch_events_for_range` /
`dedupe_and_filter_events` / `TimeframeArg` / `PlanInputs` / `parse_instrument`.
The server (worker/core/engine) is untouched and the signed intent format is
unchanged — old-style plans keep working.

**Breaking.** `trade_control_cli` no longer re-exports `BuiltCalendarBundle`,
`CalendarBarsArgs`, `CalendarBrokerArg`, `run_calendar_bars`, or
`print_summary_table`. `append_control_rules` loses its `calendar_bundles`
argument.

**Tests.** Dropped the `run_calendar_bars`-only `row_is_stale` test; rewrote
`control_rules_appended_from_pause_news_and_calendar_bundles` →
`control_rules_appended_from_pause_and_news_bundles` (no calendar bundle). 26
calendar_bars + 192 tv-arm + 21 cli-bin tests pass; clippy/fmt clean. Net −443
/ +43 lines.

**Follow-up.** Intrabar / fill-instant news gating (PR2); the pre-existing
`plan_calendar_bars_within` gap where a USD-high "blocks all instruments" event
isn't kept for a non-USD pair.

## v68 — 2026-07-07 — news/blackout windows come from the calendar, not drawn chart lines

**Why.** tv-arm resolved news/blackout windows by *drawing* pause/resume and
news-start/news-end vertical lines on the chart, then *reading them back* and
pairing sorted starts to sorted ends. Two problems: (1) TradingView snaps each
line to its bar's timestamp on readback, so a real 14:30 event on an H1 chart
came back as 14:00 — the true event minute was lost; (2) under `--start` the
starts and ends were pruned **independently** against `[start, trade-expiry]`, so
a window whose `pause` fell before the cursor but whose `resume` fell after it
had its start dropped and its end orphaned, aborting the whole arm with
`blackout lines must come in matched start/end pairs; found 1 start and 2 ends`.
The one-alert-per-drawing constraint that forced the drawn-line design is gone
(the engine is server-side Rust), so the round-trip is pure downside.

**What changed.** tv-arm now resolves windows straight from the forex-factory
calendar (`calendar_windows` → `plan_calendar_bars_within`) at **real
event-minute precision** and pushes them into the plan as a new `NewsWindow`
type — no tv-mcp draws, no readback, no pairing step. The split-pair abort and
the bar-snap precision loss are both structurally impossible now. **Drawn
pause/resume/news vertical lines left on a chart are ignored.** The calendar
event scope is the trade's own lifetime `[cursor, trade-expiry]` (cursor =
`--start`, else the last loaded bar), **not** the chart's visible area — so
scrolling/zooming the chart no longer changes which news is armed.

**Server compatibility.** The signed intent format is **unchanged** — tv-arm
still emits `pause`/`resume`/`news-start`/`news-end` `TimeReached` rules (just
sourced from the calendar), so the worker/engine enforce them exactly as before.
Already-registered old-style plans keep working.

**Breaking.** `Roles::blackout_pairs` / `news_pairs` are now
`Vec<NewsWindow>` (were `Vec<(Drawing, Drawing)>`). The drawn-line
classification arms, `in_visible_window` (for these), `auto_draw_calendar_lines`,
`draw_pair_lines`, and `pair_vertical_lines` are removed; `pair_lines.rs` keeps
only the `TimedAnchor` trait. The `BLACKOUT_*` / `NEWS_*` label vocabularies are
no longer consulted by tv-arm.

**Config.** New tv-arm flags `--news-before-hours` / `--news-after-hours`
(fractional) override the timeframe's blackout run-up / post-release buffers
(H1+ defaults 8h / 1h). `--skip-calendar-bars` still opts out entirely.

**Tests.** New `news_window` module tests (boundary ordering, sub-minute
precision, `is_past` end-inclusive); `calendar_scope_range` cursor/expiry +
empty-range tests; roles/pipeline tests updated to assert drawn lines are ignored
and windows use `NewsWindow`. 192 tv-arm + 31 trading-view tests pass. Verified
end-to-end on live EUR/USD (2 high-impact events → 2 blackout + 2 news windows).

**Follow-up (PR1b).** Retire the now-dead drawn-line CLI generators
(`CalendarBars` / `BuildPause` / `BuildNews` subcommands, `run_calendar_bars`,
`discover_calendar_bundles`) and the always-empty `BuiltCalendarBundle` arm of
`append_control_rules`. Also: intrabar / fill-instant news gating (PR2), and the
pre-existing `plan_calendar_bars_within` gap where a USD-high "blocks all
instruments" event isn't kept for a non-USD pair.

## v67 — 2026-07-07 — a reversal-close is never terminal (flattens the trade, keeps the retries)

**Why.** v64/v65/v66 all treated the question "should a `06-close-on-reversal`
retire the plan?" as turning on whether a position is open. That was the wrong
question. A reversal-close is a per-**trade** exit ("flatten the open position on
a confirming reversal"), **not** a setup invalidation. Retiring the plan on it —
even when it genuinely flattened a trade — **stops the multi-shot retries**, which
is exactly what the operator does *not* want: place → fill → close-on-reversal →
re-enter on the next signal bar is the whole point of multi-shot. Operator's
framing: "close any open trades, but don't stop the retries."

**What changed.** A `06-close-on-reversal` is now **never terminal**. It fires →
dispatches the flatten (`run_close` → `close_positions`, a no-op when flat, and
independent of the terminate decision) → and the spine stays in `AwaitEntry`. The
plan retires only via a genuine invalidation guard (`too-high` / `too-low`
invalidation veto), `trade-expiry`, or the enter's `not_after` window closing. A
single-shot enter already reaches `Phase::Done` at entry, so its reversal-close
never runs — "never terminal" is correct for both single- and multi-shot.

- **`guard_is_terminal(intent)`** collapses to `intent.action != Action::Close`
  (non-`Close` invalidation guards stay terminal; every `Close` is non-terminal).

**Breaking / reverts.** This **reverts v66's approach**: `core::position_view`
(the `PositionView` trait + `OpenSet`/`NoPositions`), the `positions: &dyn
PositionView` param on `evaluate_plan`/`evaluate_guards`, and the worker/replay
position snapshots are all **removed** — the terminate decision no longer needs
"is a position open?", so the machinery built to answer it is gone. `evaluate_plan`
loses the `positions` param. (v65's `PlanState::entry_fired_at` was already removed
in v66 and stays removed.)

**Behaviour.** Net effect vs v63 (before this whole saga): a price- or
news-windowed reversal-close fires + flattens exactly as before, but no longer
archives the plan — multi-shot plans keep re-entering. Fixes the regression where
a reversal-close could stop retries mid-setup.

**Config.** None.

**Tests.** Engine reversal-close tests assert fire + `!done` + stays `AwaitEntry`
(price and news, flat or open — the position no longer matters to termination).
Replay `open_short_closes_on_a_reversal_candle_in_the_band` now asserts the short
CLOSES ON REVERSAL **and** the (multi-shot) plan stays alive (`!r.done`). 120
engine + 786 core + 258 cli + 73 cron green.

**Follow-up.** None.

## v66 — 2026-07-07 — reversal-close terminate decision uses real position state (ends the proxy chain)

**Why.** `06-close-on-reversal` is "flatten the position *if* one is open", so
whether it retires the plan's spine depends on whether a position is actually
open. But the engine (`evaluate_plan`) is broker-free and decided `done` *before*
the cron ever consulted the broker — so it had to **guess** with a proxy. This is
the same bug fixed three times (always-terminal → window-shape → `entry_fired_at`,
v65), each proxy with a blind spot. This replaces the guessing with ground truth.

**What changed.** The engine now asks a `PositionView` whether a position is open
on the plan's instrument + direction, and a reversal-close is terminal **iff** it
is. Injected at the seam so the engine stays pure + synchronous.

- **New `core::position_view`** — `trait PositionView { fn is_open(instrument,
  direction) -> bool }`, plus `NoPositions` (flat stub) and `OpenSet` (the
  `(instrument, direction)` list both callers reduce to). `OpenPosition` carries
  no `trade_id`, so the honest question is instrument+direction, which is what the
  terminate decision needs (the engine knows `plan.direction`).
- **`evaluate_plan` / `guard_is_terminal`** gain the view. `guard_is_terminal`
  collapses to: non-`Close` → terminal; `Close` → `positions.is_open(...)`. The
  whole proxy chain (`entry_fired_at`, news/price window-shape branch) is **gone**.
- **Worker** (`trade-control-cron`) snapshots `broker.list_open_positions` once
  per tick — only for a plan carrying a reversal-close (`plan_has_reversal_close`)
  — filtered to the instrument. Broker error → flat book (non-terminal), the safe
  default (a missed terminate re-checks next tick; a wrong one archives the plan).
- **Replay** reads its existing sim-backed `ReplayBroker::list_open_positions`
  (resolves each placed attempt to open/closed as-of the cursor) into the same
  `OpenSet` — identical `PositionView` path to the worker, zero fill-logic
  duplication.

**Breaking.** `evaluate_plan` gains a `positions: &dyn PositionView` param.
`PlanState::entry_fired_at` (added in v65) is **removed** — a one-day-old proxy
with no external consumer; leaving it would be the "dead proxy lingering" trap.
Wire-compatible (was `#[serde(default)]`).

**Behaviour change.** A **news-windowed** reversal-close that fires **while a
position is open** is now terminal (it flattened a real trade → trade over). The
old "news = always non-terminal" carve-out only existed because the engine
couldn't tell open from flat; with ground truth the distinction collapses. A news
close over a flat book is still non-terminal, and still only fires inside an open
news window.

**Config.** None.

**Tests.** `core::position_view` (3). Engine reversal-close terminate tests
rewritten onto real position views (`open_plan_position` vs flat `NoPositions`):
price/news close terminal when open, non-terminal when flat; the EUR/CHF flat-book
repro. Replay `open_short_closes_on_a_reversal_candle_in_the_band` now exercises
the sim-backed view end-to-end. 120 engine + 788 core + 258 cli + 70 cron green.

**Follow-up.** None outstanding. Same-class as the shared-engine rule
(`strategy_changes_in_both_replayer_and_worker`).

## v65 — 2026-07-07 — a reversal-close before any entry no longer kills the plan

**Why.** `06-close-on-reversal` is "flatten the position *if* one is open". But
the server-side engine retired the plan on **every** price-windowed reversal-close
fire, even when **no entry had ever fired** — so a reversal candle that printed in
the SR band *before* the trade entered archived the whole plan, and the pending
enter never got its window. Caught live on EUR/CHF 2026-07-06
(`ihs-eur-chf-29ebb72b`): a `06-close-on-reversal` fired at 22:00 Brisbane, was
correctly rejected `needs-golden` (it closed nothing — the plan was still
`AwaitEntry`), yet the plan was marked `Done` and archived at that instant. The
`05-enter` (multi-shot, `max_retries: 5`) never fired. Operator's framing: "that
alert only closes an existing trade; other trades can still happen after — if it
fires when nothing is open it should only be logged."

**What changed.** A reversal-close is now terminal **only if the plan has already
fired an enter** (a position may exist → an at-band reversal is the thesis
breaking). Before the first enter fires, the close still dispatches (for
visibility / the harmless flatten) but the spine survives, so the pending enter
keeps its window. This generalises the earlier news-only carve-out (v-series
"Defect B", USD/CHF 2026-06-26) — the real discriminator is *have we entered*,
not *news vs price*.

- **New durable marker.** `PlanState::entry_fired_at: Option<DateTime<Utc>>`
  (signed/persisted, `advanced_vs`-tracked) — stamped whenever an entry rule
  fires. It's the only "have we tried to open a position?" signal the engine has,
  because a **multi-shot** enter (`max_retries > 0`) never latches into `fired`
  (it stays in `AwaitEntry` to re-enter). Exactly the EUR/CHF case.
- **`guard_is_terminal(intent, entered)`** — gains the `entered` arg. `!entered`
  → non-terminal regardless of window shape. `entered` → prior rule (news-only
  non-terminal, price-windowed terminal).
- **Shared engine** — both the live worker cron and the offline `replay-candles`
  sim get the fix (one `evaluate_plan`). The replay's reversal-close post-pass
  already only flattens an *open* position, so a pre-entry fire that closes
  nothing is a no-op there too — consistent, no replay-side change.

**Breaking.** None (wire-compatible: `entry_fired_at` is `#[serde(default)]`).

**Config.** None.

**Tests.** `price_close_before_any_entry_fires_but_is_non_terminal` (the EUR/CHF
repro), `news_close_stays_non_terminal_even_after_an_entry` (the `entered` gate
must not make news-closes terminal); the two prior price-close terminal tests now
seed an entry first (`seed_entered_at`). 120 engine + 785 core green.

**Follow-up.** None. Same-class as the news carve-out and the shared-engine rule
(`strategy_changes_in_both_replayer_and_worker`).

## v64 — 2026-07-06 — entry SL-spread floor: mean spread over a trailing window (one shared bar provider)

**Why.** The entry SL-vs-spread floor (`sl_distance ≥ 10 × spread`) sized off a
**single** spread sample — one live `get_quote` on the worker, the fire bar's
`ask_c − bid_c` in replay. Entry candles are often spiky (high volatility → a
momentarily wide spread on that exact bar), so a one-off spike blew the 10×
floor out and widened the stop far past the instrument's normal spread. Observed
on live demo entries where the placed stop was noticeably wider than warranted.

**What changed.** The floor now sizes off the **mean `ask − bid` over the last N
candles** (default **5**, per-trade tunable), and the worker and the offline
replay reach that window through the **same bar provider** so they can't drift.

- **One shared provider + reducer.** `Broker::get_bidask_candles` (new trait
  method, default no-op) is the single window source; `core::broker::
  trailing_spread_mean(candles, window)` is the single reducer (mean of the last
  N bars' close spread, degenerate samples dropped). Real impls: OANDA (keeps the
  `MBA` bid/ask it already fetches) and TradeNation (three `PriceType` fetches
  zipped by timestamp).
- **Worker** (`run_enter`): `windowed_entry_spread` fetches the last N via
  `get_bidask_candles` and reduces with `trailing_spread_mean`; feeds the floor.
  Fails open to a single live `get_quote` when the window is unavailable (no plan
  granularity, fetch error, or all-degenerate window) — the pre-window behaviour.
- **Replay**: `ReplayBroker` now implements `get_bidask_candles` from its own
  recorded series (bounded at `as_of`), so the replay's entry **gate** runs the
  identical `run_enter` + windowed floor. The displayed bracket / simulated exit /
  System-2 baseline read the same windowed mean off `Fire.entry_spread_price`.
- **Engine**: `apply_entry_spread_floor` gains `entry_spread_price: Option<f64>`
  (prefer the windowed mean; fall back to the fire bar). `simulate_fill_windowed`
  + `widened_stop_at` thread it; `simulate_fill` stays a `None` wrapper so all
  prior sim tests are byte-identical.
- **tv-arm**: `--spread-window N` bakes onto the signed `05-enter` intent.

**Config.** New signed `Intent.spread_window: Option<u32>` (default absent →
worker default 5) + `TradeSpec.spread_window` + `tv-arm --spread-window`. System
2 (spread-hour transient widen) is **unchanged** — this is only the System-1
entry floor.

**Breaking.** `Broker` gains `get_bidask_candles` (default-impl'd, so existing
impls compile). `apply_entry_spread_floor` / `widened_stop_at` gained an
`entry_spread_price` arg; `simulate_fill` unchanged (new `simulate_fill_windowed`
carries the window).

**Tests.** `trailing_spread_mean` (mean / window>series / empty / spike-damp),
`mean_spread` (skip-degenerate / spike-damp), engine
`entry_floor_prefers_supplied_window_spread_over_fire_bar`, tv-arm
`build_trade_from_spec_{default,custom}_spread_window`. Full workspace green
single-threaded (one annotate test is a pre-existing `HOME` env race under the
parallel runner).

## v63 — 2026-07-06 — replay: gap-through fills, native position-tool annotation, window clamp; Cloudflare fully retired

**Why.** A batch of replay-fidelity fixes plus the environment/tooling cleanup
now that both dev and staging run as local native/Postgres workers.

**What changed.**
- **Gap-through fills (replay).** The offline fill simulator decided a
  stop/limit filled with a *containment* test (`lo ≤ level ≤ hi`), which missed a
  fill when price **gapped through** the trigger (whole bar range on the far
  side). Replaced with a directional touch (`book_reaches` +
  `Approach::{FromBelow,FromAbove}`) on all six touch sites (entry + SL/TP exits).
  Replay-only — the live worker places a real broker stop. Found on AUD/NZD
  `ihs-aud-nzd-37d31f02` (was `NEVER FILLED`, now fills → +1.09R).
- **Native position-tool annotation.** `replay-candles --annotate` now draws each
  filled position as TradingView's **native long/short position tool** (green
  profit / red stop zone) + a small outcome label, instead of two rectangles.
  The tool *is* creatable via tv-mcp after all — `createShape`'s promise must be
  awaited, and stop/profit are mintick-derived tick offsets. Re-run cleanup moved
  from a `replay:` text prefix to a sidecar entity-id manifest
  (`~/.config/trade-control/replay-annotations.json`), since the tool has no text
  field. Hand drawings untouched.
- **Future-window clamp.** A plan whose trade-expiry is still in the future asked
  OANDA for candles that don't exist yet ("Invalid value for 'from'. Time is in
  the future"). `pull_end` now clamps to the last closed bar.
- **Cloudflare fully retired (docs).** README + CLAUDE.md updated: both envs are
  native local Postgres workers now; Oracle is the long-term target, blocked on
  OKE compute.
- **Dead Pine plumbing removed.** Signal detection is fully server-side Rust
  (`core/src/signals/`, `PinePattern`); the `BAKED_PINE_NAME` / `ENV_PINE_NAME` /
  `TRADE_CONTROL_PINE_NAME` study-name plumbing had zero consumers and was ripped
  out (`deploy_env`'s `native` marker is now the 5th arg).

**Tests.** New: simulator gap-through regressions (3), `position_overrides`,
annotate manifest/box-style/outcome-label. All suites green (256+70+21 cli,
117+3 engine, 38 trading-view); no golden fixture shifted.

**Breaking.** `deploy-lib.sh`'s `deploy_env` drops the `<pine-name>` positional
(worker-kind marker moves 6th → 5th). Affects only the deploy scripts.

---

## v63 also folds in — 2026-07-05 — replay journal: reconcile the SL lines + tag entries with an id

**Why.** On EUR/AUD `hs-eur-aud-3d0b5dda` the enter block printed **three
different stop-losses that didn't reconcile**: `order: SL 1.65787`, then "widened
to 1.65458 (from 1.65238)", then "restored to 1.65238" — the widen/restore moved
to *tighter* levels than the placed stop. Cause: the `order:` line applied the
System-1 entry spread floor (fire-bar spread 8.3p → 10× floor → SL 1.65787) but
`widened_stop_at` (System 2) re-resolved from scratch and used the **un-floored
signed SL** (1.65238) as its baseline. Two systems, two baselines, one confusing
journal. Also, the widen/restore notes were undated-looking lines bunched under
the entry bar, with no way to tell which entry they belonged to when several
enters fired.

**What changed.**
- **One baseline.** New shared `engine::apply_entry_spread_floor` (returns the
  floored stop + the spread used, or a reject) replaces the ad-hoc floor in
  `simulate_fill` *and* is now applied inside `widened_stop_at`, so the placed
  stop, the simulated exit, and System 2's widen/restore all key off the **same**
  floored level. On the worked trade the lines now read: order 1.65787 → widened
  1.66007 → restored 1.65787 — monotonic and correct.
- **Show the spread.** The `order:` line gains a "SL floored to 10× spread
  (Np @ entry bar)" note when the floor moved the stop, and the System-2 widen
  line shows the widen-bar spread ("…, Np spread…"). No more mystery-wide stops.
- **Entry ids + per-bar events.** Each enter fire is tagged `[entry #N]`, and its
  time-based sub-events (break-even, widen, restore) now read
  `entry #N @ <their own bar>: …` — a mini timeline under the entry instead of
  undated lines. `SpreadWiden` gains `widen_spread_pips`.

**Compatibility.** Replay reporting only — no signed field, no live-worker
behaviour change. The entry-floor *logic* is unchanged (still fire-bar spread,
per the operator's call); this only reconciles the display and threads the id.

**Tests.** `widened_stop_at_baseline_is_the_floored_stop_not_the_signed_sl`
(guards the reconciliation); existing widen tests unchanged (their signed SLs
already clear the floor, so flooring is a no-op).

## Unreleased — 2026-07-05 — replay shows the System-2 widen as *transient* (widen + restore)

**Why.** The NY-close stop-widen (System 2) is **transient live**: the recovery
watcher (`trade-control-cron/src/blackout_watch.rs`) restores the original stop as
soon as the spread recovers (≤4p) or a 3h backstop fires. But the replay printed
only the *widen* line and no *restore*, and (because `widened_stop_at` is a
display-only annotation, never applied to `simulate_fill`'s exit) the journal read
as a permanent, un-reverted stop inflation. Raised by the operator on EUR/AUD
`hs-eur-aud-3d0b5dda` (demo-journal
`BUG-ny-close-widen-is-permanent-not-transient.md`): "why does a temporary
NY-close protection permanently widen my stop and never put it back?" The live
worker already does put it back — the replay just didn't say so.

**What changed.**
- `engine::simulator::SpreadWiden` gains `restored_at: Option<DateTime<Utc>>`,
  reconstructed by a new `restore_bar` helper that mirrors the live watcher's two
  restore triggers: the first post-widen bar with spread ≤
  `SPREAD_BLACKOUT_RECOVERED_PIPS` (4p, clock-agnostic), else the
  `BLACKOUT_BACKSTOP_SECONDS` (3h) backstop.
- The replay report now prints a matching **"SL restored to … @ …"** line (or
  "still widened at window end" when neither trigger lands), and the widen line is
  relabelled `note:` and marked *transient* + *informational only* (the simulated
  exit uses the un-widened bracket).

**Refactor.** `BLACKOUT_BACKSTOP_SECONDS` moved from
`trade-control-cron::constants` into `trade_control_core::spread_blackout` (beside
the recovered/elevated cutoffs — spread-blackout tuning now lives in one place) so
the engine can compute the same backstop without depending on the cron crate. The
cron `constants` module re-exports it, so every existing consumer is unchanged.

**Compatibility.** Replay reporting only — no signed field, no live-worker change
(live was already transient). No behaviour change to any simulated exit.

**Tests.** `widened_stop_at_reports_restore_on_spread_recovery`,
`widened_stop_at_restore_is_none_when_spread_stays_wide`,
`widened_stop_at_restore_fires_on_the_backstop`.

## Unreleased — 2026-07-05 — replay System-2 stop-widen gates on the NY-close edge

**Why.** The live spread-blackout stop-widen (System 2,
`trade-control-cron/src/blackout_apply.rs`) fires **only at the NY-close edge**
(21:00 UTC under EDT / 22:00 under EST), where the post-close spread blowout it
guards against occurs. The offline replay's mirror
(`engine::simulator::widened_stop_at`) had **no such gate** — it widened on the
*first* post-fill bar whose spread crossed the trigger, at any hour. On a trade
whose spread flared away from the NY close, the replay would widen a stop the
live worker would have left alone: a replay-vs-live divergence.

Surfaced by the EUR/AUD journal bug `hs-eur-aud-3d0b5dda` (demo-journal
`BUG-replay-sl-widen-uses-wrong-bar-spread-and-synthetic-candles.md`). That
report's two headline claims were misdiagnoses — the widen it flagged fired on
the **NY-close bar** (01-Jul 21:00 UTC), on **real** OANDA `price=MBA` bid/ask
(the replay already pulls real books) — but it half-found this real gap.

**What changed.** `widened_stop_at` now `continue`s past any post-fill bar where
`trade_control_core::ny_clock::is_ny_close_edge(c.time)` is false, so the replay
widens only on the NY-close bar — identical to the live cron. Shared engine ⇒
worker (wasm) and replay can't drift.

**Compatibility.** No signed-field or schema change. Behaviour only changes for a
replay whose open-position spread crosses the widen trigger on a non-NY-close bar
(previously widened, now correctly ignored). Trades whose wide bar *is* the
NY-close bar (the common case, incl. `hs-eur-aud-3d0b5dda`) are unaffected.

**Tests.** `widened_stop_at_ignores_a_wide_bar_off_the_ny_close_edge`,
`widened_stop_at_widens_on_the_ny_close_bar_not_the_first_wide_bar`;
`widened_stop_at_reports_the_widen_bar_for_a_long` moved onto 21:00 UTC (it
previously used 12:00 UTC, silently encoding the pre-fix "any bar" behaviour).

## Unreleased — 2026-07-05 — break-and-close reads a buffered *zone*, not the bare line

**Why.** The `OnClose` cross (break-and-close, invalidation caps) required a
*strict* cross of the raw line against the single prior processed close:
`prev > level && close <= level` (down). A candle that dipped **into the zone of
the line** and closed a hair on the near side poisoned the next bar's
`prev_close`, so an obviously-clean close-through was silently rejected. Caught
on NAS100 short iH&S 2026-07-02 (`hs-nas100-usd-514640f9`): the 15:00 (3pm)
candle closed at **29844.2**, 0.9 pt *above* the neckline (29843.3); the 16:00
(4pm) candle broke down hard (close **29713.4**) but `prev_close 29844.2 >
neckline 29844.3` was **false** by 0.1 pt → the break never stamped, killing the
whole retest→enter spine.

**What changed.** `level_crossed`'s `OnClose` arm now measures against the
buffered **zone** `[level - buffer, level + buffer]` (the same
`cross_buffer_pct` already used by the intrabar arm, default 0.02%). A
directional break fires when the close lands past the **far zone edge** and the
prior close was **not already past that edge**:
- **Down** — `prev > level - buffer && close <= level - buffer`
- **Up** — `prev < level + buffer && close >= level + buffer`
- **Either** — a close past either edge.

Because the prior-close guard uses the *zone edge*, a candle that only dips into
the zone on the near side no longer pre-arms the guard and blocks the next bar's
genuine close-through. On the NAS100 chart the break now stamps **at 4pm**,
exactly where the chart breaks. Applies to **all** `OnClose` crosses (the
too-high invalidation cap too — consistent, single code path). Lives in the
shared engine, so worker (wasm) + `replay-candles` pick it up identically.

**Compatibility.** With `buffer 0.0` (`upper == lower == level`) the arm is
byte-identical to the old raw-line comparison — plans without `cross_buffer_pct`
are unchanged. Inclusivity mirrors the old rule (`prev < edge && c >= edge`).

**Tests.** `on_close_zone_break_registers_after_near_side_dip` (the fix — a
near-side dip then a genuine break the next bar),
`on_close_zone_close_inside_zone_does_not_fire` (a close into but not past the
zone is not a break), `on_close_zero_buffer_is_raw_line` (0.0 buffer =
old behaviour). Renamed `cross_buffer_ignored_by_either_and_on_close` →
`cross_buffer_ignored_by_intrabar_either` (OnClose no longer ignores the
buffer). Verified end-to-end: the NAS100 replay stamps break-and-close at 4pm
and progresses the spine to the retest.

## Unreleased — 2026-07-04 — retest no longer gated behind an in-window break-and-close

**Why.** A `--skip-break-and-close` plan (the operator armed *on* the retest,
after the neckline break already happened) carries no `03-prep-break-and-close`
rule, so `break_close_at` was never stamped. `stamp_retest` short-circuited on
`break_close_at == None` and `retest_satisfied` returned false — so the retest
(hence the entry) could **never** fire, regardless of price action. Silently
forfeited a would-be winner on NZD/SGD `ihs-nzd-sgd-7b24d14c` (2026-06-30 iH&S
long: the retest tapped the neckline at 17:00 but never stamped; price later ran
through TP).

**What changed.** A plan with **no** break-and-close rule is treated as
break-already-satisfied by construction. New `effective_break_at(plan, state,
window)` in the shared engine returns:
- the stamped `break_close_at` when present (unchanged);
- for a no-break-rule plan, a floor one second before the window's first candle,
  so every in-window bar is eligible to stamp the retest (and the first bar
  counts as `N=1` in the tolerance decay);
- `None` (retest stays gated) when a break rule is present but hasn't fired —
  the break genuinely mustn't be assumed.

`stamp_retest` and `retest_satisfied` both key off this. Since it lives in the
engine, the worker (wasm) and offline `replay-candles` pick it up identically —
no drift.

**Scope (deliberate).** This fixes only the *no-break-rule* case
(`--skip-break-and-close`). A modern-spine plan that *has* the break rule but is
replayed/armed **after** its break bar stays gated by design — the remedy there
is to re-arm with `--skip-break-and-close`. (Operator decision.)

**Breaking.** `retest_satisfied` now takes `&TradePlan` as its first argument
(engine-internal).

**Tests.** `skip_break_and_close_plan_stamps_retest_and_enters` (retest + enter
fire with `break_close_at` never stamped),
`break_rule_present_but_unfired_keeps_retest_gated` (the negative guard).
Verified end-to-end on the NZD/SGD replay: the retest now stamps and the entry
gate opens; a synthesised modern-spine variant still stamps break-before-retest
in order.

## Unreleased — 2026-07-04 — replay-candles `--start`/`--end` accept Brisbane + explicit offsets

**Why.** `replay-candles` renders every candle/fill/exit in Brisbane time
(UTC+10), and the operator works in Brisbane, but the `--start`/`--end` window
flags parsed a bare datetime as **UTC** — so the input read differently from the
output, and passing a timezone (`...+10:00`) failed outright with "is not a valid
UTC datetime".

**What changed.** `parse_start_end` (was `parse_utc`):

- A **bare** datetime (no offset) is now interpreted as **Brisbane (UTC+10, no
  DST)** — matching the tool's own output zone. `--start 2026-06-30T17:00` is
  17:00 Brisbane = 07:00 UTC.
- An **explicit offset or `Z`** is honoured as written: `...T07:00Z` = UTC,
  `...T17:00+10:00` = Brisbane spelled out, any offset respected. Both minute and
  second precision accepted on every form (RFC3339's mandatory-seconds gap is
  covered by a `%z` fallback + trailing-`Z` normalisation).

**Breaking.** None on the wire; a script that relied on bare `--start`/`--end`
being **UTC** now shifts 10h earlier — append `Z` to keep the old UTC reading.

**Tests.** `bare_datetime_is_brisbane_minute_and_second_precision`,
`explicit_offset_is_honoured` (Z / +10:00 / +02:00), `rejects_garbage_datetime`;
two window-resolution tests pinned to `...Z` to stay zone-independent.

## Unreleased — 2026-07-03 — time-decaying retest tolerance

**Why.** The retest required the wick to *reach* the neckline on every bar. The
original author's insight: a retest right after the break should be tight, but
the further price drifts in time the less its exact distance to the neckline
matters. So the closeness requirement should **loosen as bars pass** since the
break-and-close.

**What changed.** The retest cross now carries a per-bar decaying **near-side
tolerance**. With `N` = bars since the break-and-close (first bar after = 1) and
`ATR` the Wilder ATR at the bar:

```
tolerance(N) = (N - 1) × retest_atr_step × ATR
```

- Bar 1: tolerance 0 → the wick must **reach** the neckline (unchanged).
- Each later bar: `+ retest_atr_step × ATR` of slack — a wick that comes *within*
  the tolerance of the line stamps the retest even without reaching it.

`retest_atr_step` defaults to **0.075** (≈1 ATR of slack by ~bar 14). Implemented
in `engine/src/evaluate.rs` (`stamp_retest` → `retest_tolerance` +
`retest_crossed`); ATR comes from the engine's existing `wilder_atr` over the
detector window (`atr_length_for(granularity)`) and **hard-fails** if absent
(structurally unreachable by the retest phase — a `None` signals a mis-sized
window, surfaced loudly rather than silently papered over as tolerance 0).

**Config.** New signed field `TradePlan.retest_atr_step: f64` (serde-default
0.075, `DEFAULT_RETEST_ATR_STEP`). New `tv-arm --retest-atr-step <f64>` bakes it
per-trade; absent → the default. Only the **retest** uses it; `cross_buffer_pct`
(which *tightens*) still governs the other intrabar consumers.

**Behaviour.** Strictly *more permissive* for the retest after bar 1: near-misses
that used to be ignored now stamp once enough bars have passed. Bar-1 behaviour
and every non-retest cross are unchanged.

**Breaking.** `build_trade_plan` gains a `retest_atr_step: f64` parameter; every
`TradePlan { … }` literal gains the field (serde-default keeps old plans valid).

**Shared.** Engine rule → live worker tick + replay simulator both follow (one
edit, no drift). tv-arm bakes the signed field.

**Tests.** engine: bar-1-must-reach, later-bar-near-miss-within-tol fires,
beyond-tol rejects, tolerance-grows-linearly (4 new). tv-arm:
retest_atr_step-carried-onto-plan. engine 102 + tv-arm 189 green.

**Follow-up.** Visual tuning of `retest_atr_step` on past charts (operator).

## Unreleased — 2026-07-03 — intrabar cross is a pure straddle (high/low opposite sides)

**Why.** The retest (and every other intrabar cross) required the bar to have
*opened* on the far side of the line and wicked through — "came from the open
side". The operator wants the retest more permissive: a bar counts as crossing
the neckline whenever its **high and low sit on opposite sides** of the line,
regardless of where it opened or closed. A bar that opens on the near side, dips
through, and comes back is a genuine touch on a tick timeline.

**What changed.** `level_crossed`'s `BarEvent::Intrabar` arm
(`engine/src/evaluate.rs`) dropped the `open`-side guard:
- `Up`   was `open <= level && high >= level + buffer` → now `high >= level + buffer`
- `Down` was `open >= level && low  <= level - buffer` → now `low  <= level - buffer`

The `straddles` precondition (`low <= level <= high`) is unchanged and now carries
the "opposite sides" requirement on its own; the directional wick + `buffer`
(`cross_buffer_pct`) still gate a one-tick graze. `Either` and `OnClose` arms
untouched — **break-and-close still requires a genuine close through the line**.

**Behaviour.** Strictly *more* firings: any bar that used to fire still fires
(open-side bars straddle too), plus bars that opened on the near side and wicked
through now fire. Affects the retest (`04-prep-retest`), the `too-low`
invalidation floor, the computed-fib pcl levels, and M/W cancel/overshoot vetos —
every consumer of the intrabar arm. The literal `too-high` cap is `OnClose`
(close-confirmed) and is unaffected.

**Breaking.** None — pure engine decision, no signed field or API change.

**Shared.** The rule lives in the shared `engine` crate (`level_crossed`), the
single source of truth for both the live worker's engine tick and the offline
`replay-candles` simulator (both route through `evaluate_plan`/`eval_trigger`) —
one edit, no replayer/worker drift.

**Tests.** Renamed + flipped the two open-side intrabar tests to assert
any-straddle-fires (`intrabar_fires_on_any_straddle_regardless_of_open`,
`intrabar_down_fires_on_any_straddle_regardless_of_open`); buffer + real-data
AUD/JPY retest tests unchanged (their bars open on the far side anyway). engine 98
green.

**Follow-up.** None.

## Unreleased — 2026-07-03 — SL-spread salvage widens to exactly 10× (was 11×)

**Why.** The SL-vs-spread salvage widened a too-tight stop to `11 × spread` — a
deliberate margin above the `10×` floor. The operator wants the salvage to move
the stop only as far as strictly necessary: to the floor itself, `10 × spread`,
and no further. A stop pushed to `11×` gives away an extra `1 × spread` of risk
on every salvaged entry for no rule benefit.

**What changed.** `SL_WIDEN_SPREAD_MULTIPLE` in
`core/src/intent/sl_spread_floor.rs` is now `= SL_MIN_SPREAD_MULTIPLE` (10.0)
instead of `11.0`. A salvaged stop lands exactly on the floor. This is safe:
`sl_spread_floor_violation` uses a strict `<` (`sl_distance < 10 × spread`), so a
stop at exactly `10 × spread` is **not** a violation, and the widened distance is
the same `10.0 × spread` multiplication as the check — bit-identical, no
floating-point boundary risk.

**Behaviour.** Salvaged entries now enter with a slightly *tighter* (more
favourable) stop and correspondingly *higher* R than before. Entries that were
salvageable at 11× remain salvageable at 10× (the R only improves). The
reject/accept boundary shifts marginally: a trade whose R at 11× was just under
`min_r` may now clear it at 10× and enter. Reject and log messages print `10x`
automatically (built from the constant).

**Breaking.** None — the constant is internal; no signed-field or API change.

**Shared.** Pure `core` helper, so the live worker (`run_enter`) and the offline
replay simulator (`engine/src/simulator.rs`, which calls the same
`widen_sl_to_spread_floor`) both follow with one edit — no replayer/worker drift.

**Tests.** Updated the 11×-hardcoded expectations in `sl_spread_floor.rs` (3
widen tests) and `simulator.rs` (2 tests; `widened_sl_protects_the_leg`'s dip bar
moved from 1.0990→1.0994 since the widened SL is now 1.0990, not 1.0984). core
764 + engine 98 green.

**Follow-up.** None.

## Unreleased — 2026-07-02 — tv-arm: neckline serves the retest (one drawing)

**Why.** The `03-prep-break-and-close` and `04-prep-retest` rules cross the
*same* neckline — the retest is by definition a cross back through it (opposite
direction, intrabar). They only lived as two separate drawings because
TradingView couldn't fire two alerts off one trendline. That limitation is gone,
so making the operator draw the neckline twice is pure ceremony.

**What changed.** `tv-arm`'s role resolution (`tv-arm/src/roles.rs`,
`resolve_retest`) now reuses the resolved neckline (`break_and_close`) for the
retest role **when no `retest` trendline is drawn** — `04-prep-retest` gets the
identical geometry via the existing opposite-direction/intrabar trigger build,
unchanged. A separately-drawn `retest`/`neckline-retest`/`retrace` line is still
honoured (backward compat) but now **warns as deprecated**. `check_required`
(`tv-arm/src/pipeline.rs`) no longer demands a standalone `retest` line — the
neckline satisfies both; the retest is only *independently* required when the
neckline itself is skipped (`--skip-break-and-close` without `--skip-retest`).

**Breaking.** None. A chart with a separate retest line behaves as before (plus a
deprecation warning); a chart with only a neckline now arms the retest too
instead of erroring `missing … 'retest'`.

**Config.** No new fields. Same labels (`neckline`/`break-and-close`;
`retest`/`neckline-retest`/`retrace` still recognised for the deprecated path).

**Tests.** `tv-arm/src/roles.rs`: `neckline_serves_the_retest_when_no_retest_line_is_drawn`,
`a_separate_retest_line_is_still_honoured_deprecated`,
`no_neckline_and_no_retest_leaves_the_retest_role_empty`. tv-arm 188 green,
clippy + fmt clean.

**Follow-up.** The `retest`/`neckline-retest`/`retrace` label vocabulary and the
`--skip-retest` flag can eventually be retired once no live charts draw a
separate retest line.

## Unreleased — 2026-07-02 — tv-arm: rename `--update` → `--replace` (alias kept)

**Why.** The re-arm flag was named `--update`, which reads like an in-place
patch. It isn't: it *deletes* the prior plan and registers a brand-new one under
a fresh `trade_id` with blank engine state (phase, vetos, seen-ids,
entry-attempts, news/blackout windows are all `trade_id`-keyed, so nothing
carries over). `--replace` names what actually happens.

**What changed.** `tv-arm`'s `--update` flag is renamed `--replace`
(`tv-arm/src/args.rs`); `--update` stays as a **`visible_alias`** so existing
scripts and muscle memory keep working. Behaviour is byte-identical. The field
`Args.update` → `Args.replace`; the helpers `resolve_update_target` →
`resolve_replace_target` and `update_existing_plan` → `replace_existing_plan`
(`tv-arm/src/pipeline.rs`); log lines and the ambiguous-target error now say
`--replace`. The `--help` text also spells out the replace-not-patch semantics
and the strand-a-live-order/position caveat.

**Breaking.** None for the CLI surface (alias preserves `--update`). Internal
fn/field renames only — no external callers.

**Config.** None.

**Tests.** args: `--replace` bare + with-target parse, and `--update` alias
parses into the same field. pipeline: the 5 `resolve_*_target` tests renamed
`replace_*`, plus the ambiguous-target error now asserts it names `--replace`.
tv-arm 185 green; clippy + fmt clean.

## Unreleased — 2026-07-02 — replay report: Net R + $100k-account P&L projection

**Why.** The `replay-candles --simulate` report tallied only raw TP/SL counts.
Reading a window's actual profitability meant summing R multiples by hand and
guessing what the sequence would have made on a real account. The operator wants
the bottom line — net R and a dollar figure — right in the report, and each fill
to show its own contribution.

**What changed.** `cli/src/bin/replay_candles/report.rs` now scores every *taken*
fill's realized R multiple (`realized_r` = `(exit − entry) / (entry − SL)`,
correctly signed for both directions off the protected stop after the
spread-floor widen) and compounds it into a simulated **$100k account at 1% risk
per trade** (`Tally`). Two new outputs:

- **Per fill:** a `R: +N.NN  |  $100k acct (1% risk): +PNL → $BALANCE` line under
  each TP / SL / reversal-close fill, showing that trade's R and the account after
  it. Not-taken kinds (never-filled, declined, gate-blocked, superseded)
  contribute 0R and print nothing.
- **Summary footer:** `… | Net R: +N.NN | $100k acct (1%/trade): $BALANCE (+PROFIT)`
  appended to the existing `TP:/SL:` line.

The account compounds — each trade risks 1% of what's *left*, so a losing streak
shrinks the per-trade stake and a winning one grows it. `Net R` is the plain sum
of per-trade R multiples (account-independent). A zero-risk bracket (SL at entry)
scores 0R rather than dividing by zero.

**Breaking.** `render_fire`'s three `&mut usize` counters collapse into one
`&mut Tally` (private fn — no external callers).

**Config.** None. `--simulate`-only output; the $100k / 1% constants are fixed.

**Tests.** 8 new in `report.rs`: `realized_r` sign/scale/zero-risk across both
directions; `Tally` compounding + summary line; `book_and_render_r` no-op without
a stop and its win-scoring path. cli 22 report-tests green.

## Unreleased — 2026-07-02 — --start: invalidation picks the line on the correct side of the neckline

**Why.** A chart can carry two same-labelled invalidation lines — the real one
plus a stale leftover from an earlier trade at a different price. Under `--start`
the invalidation slot broke ties purely by anchor *time* (nearest either side of
the cursor), so a stale `too-low` whose timestamp happened to sit nearer the
cursor out-ranked the real floor. On AUD/JPY (IH&S long, 2026-06-29) the real
`too-low` floor was 111.288 (below entry) but a stale one at 112.993 (above
entry, above the neckline, above TP) won the slot — and because that level is
baked onto the enter as a continuous at-entry veto (Bug #12), *every* `05-enter`
fire was rejected `veto-active (too-low)` and the plan never filled.

**What changed.** `classify` (`tv-arm/src/roles.rs`) now resolves the neckline
(`break_and_close`) *before* the invalidation, and passes its mean price into
`pick_slot_with_label`. Under `--start` a **side-of-neckline filter** runs before
the nearest-to-start tiebreak: a `too-low` floor must sit **below** the neckline
and a `too-high` cap **above** it (per each candidate's own label), so a
wrong-side stale line is dropped. The filter falls back to the full candidate set
when it would drop everything (no neckline, or all candidates on the wrong side)
so a genuinely unusual chart is never stranded. Window-scoped modes
(`LatestWins` / `WindowAware`) are unchanged — their visible-window filter
already handles staleness.

**Breaking.** `pick_slot_with_label` gains a `neckline_ref: Option<f64>` param
(private fn — no external callers).

**Config.** None. `--start`-only behaviour.

**Tests.** `nearest_to_invalidation_drops_too_low_above_neckline` (the bug: stale
above-neckline `too-low` out-times the real floor → real floor wins),
`nearest_to_invalidation_drops_too_high_below_neckline` (short mirror), and
`nearest_to_invalidation_side_filter_falls_back_when_all_wrong_side` (fallback
never strands the setup). tv-arm 183 green.

**Follow-up.** None.

## Unreleased — 2026-07-02 — --start: stray M/W path no longer hijacks an H&S arm

**Why.** A chart can carry a leftover M/W `path` drawing from an earlier setup
while the operator journals an H&S. Under `--start`, the 3-anchor path relax rule
(`start >= left-shoulder B`) matched *any* old path whose left shoulder predated
the cursor — and a path's mere presence routes the whole arm to M/W
(`pipeline.rs` keys on `roles.mw_path`). So a 12-Jun path hijacked a 29-Jun H&S
arm (AUD/JPY 2026-06-29): tv-arm resolved the M/W path instead of the drawn
neckline.

**What changed.** `pick_mw_path` (`tv-arm/src/roles.rs`) now takes the resolved
H&S neckline anchor and, under `--start` only, compares it to the chosen path's
own neckline anchor `C (points[2])`: whichever is anchored **nearer `start`**
wins. If the drawn neckline is nearer, the path is dropped (`None` → H&S arm).
This is what makes the permissive 3-anchor relax rule safe. `classify` threads
`roles.break_and_close.latest_time()` into the picker (neckline is resolved
before `mw_path`, so it's available).

**Breaking.** `pick_mw_path` gains a `neckline_anchor: Option<i64>` param
(private fn — no external callers).

**Config.** None. `--start`-only behaviour; the window-scoped modes
(`LatestWins` / `WindowAware`) are unchanged.

**Tests.** `nearest_to_drops_stray_path_when_hs_neckline_is_nearer_start` (the
bug: stray path far left + neckline near start → `mw_path == None`, H&S arm) and
`nearest_to_keeps_path_when_it_is_nearer_start_than_neckline` (mirror: a genuine
M/W journaling arm with a nearer path is not demoted). tv-arm 180 green.

**Follow-up.** None. The `--cross-buffer-pct` arm-time flag is still separate/unbuilt.

## Unreleased — 2026-07-02 — invalidation: drawn line = close-confirm (both directions), fib = wick

**Why.** The invalidation cross-mode was tied to the *name* (`too-high` →
close-confirm, `too-low` → wick), which was only correct for short trades. For a
**long** trade the `too-low` is the human's **drawn line** (below the
shoulder/head) and must be close-confirmed — but it was baked as `Intrabar`
(wick), so a 7am wick that closed back above the floor wrongly invalidated the
setup (AUD/NZD long, 2026-07-01: too-low fired on a wick, killing the plan before
the entry).

**The correct rule (operator).** The two invalidation guards are two different
*kinds*, and the kind — not the direction — decides the cross-mode:

- **The drawn line** (human-annotated: `too-low` for a long, `too-high` for a
  short) is **close-confirmed** (`OnClose`) in *both* directions — "the candle
  opened one side of my line and closed the other" is a genuine break; an
  intrabar spike that closes back does not invalidate.
- **The computed fib / pcl level** (the opposite name: `too-high` for a long,
  `too-low` for a short — "the power of the setup is consumed") is a
  **wick-through** (`Intrabar`, `Either`): any straddle aborts.

Direction only decides *which way* the drawn line is crossed (`Down` into the
long floor, `Up` into the short cap), not the confirm mode.

**What changed.** `invalidation_or_pcl_trigger` (`tv-arm/src/trade_plan_build.rs`)
now emits the drawn-line branch as `HorizontalCross { bar: OnClose }` for both
directions (was `Intrabar` for long). The fib/pcl branch is unchanged
(`PriceValueCross { dir: Either, bar: Intrabar }`). This **supersedes** the
2026-07-01 asymmetry where only the short `too-high` cap was reverted to
`OnClose`.

**Build-site only.** The engine's `OnClose`/`Intrabar` arms are unchanged; this
only changes which `BarEvent` tv-arm bakes. `replay-candles` reads the same plan,
so replay and the live worker both pick it up with no engine change.

**Tests.** `ihs_long_too_low_invalidation_is_close_confirmed` (was
`…stays_intrabar_wick`, flipped to assert `OnClose`);
`builds_hs_short_rules_with_correct_triggers` still asserts the short cap
`OnClose`. tv-arm 178 green; clippy + fmt clean.

## Unreleased — 2026-07-01 — bake `--start` into the plan for self-consistent replays

**Why.** `tv-arm --start` only affected *arming* (which drawings to pick). The
offline `replay-candles` harness still derived its window *start* from the
TradingView chart's replay cursor — so even a correct `--start` arm failed if the
chart's cursor wasn't lined up before the trade start. Concretely: a June-12
`--start` arm produced a plan with expiry 22 Jun, but replay read the chart
cursor at 22 Jun 16:00 (after the expiry) and errored `end before start`. The two
tools didn't share the cursor.

**What changed.** `tv-arm` now bakes the arm-time `--start` onto the plan as a new
optional field `TradePlan.replay_start` (a Unix second). `replay-candles` prefers
it for the window start: **`--start` flag → `plan.replay_start` → chart cursor**.
So a `--start` journaling arm carries *both* ends of its window (start =
replay_start, end = trade-expiry) and replay is fully deterministic — the TV chart
position no longer matters. An arm without `--start` is unchanged (field omitted,
chart cursor used as before).

**Field.** `replay_start: Option<i64>`, `#[serde(default, skip_serializing_if =
"Option::is_none")]` — omitted from the JSON entirely when absent, so pre-field
plans round-trip byte-clean. Signed as part of the whole-body HMAC like every
plan field. **The worker does not act on it** — it's a journaling aid the engine
ignores.

**Tests.** core: `missing_replay_start_defaults_to_none_and_is_omitted`,
`replay_start_round_trips`. replay-candles: `window_start_comes_from_plan_replay_start`,
`window_start_flag_overrides_plan_replay_start`. core 761 / cli 256 / replay-candles
59 / tv-arm 178 / engine 97 green; clippy + fmt clean.

## Unreleased — 2026-07-01 — tv-arm `--start`: whole-chart, nearest-to-start arming

**Why.** Journaling a replay trade meant putting TradingView in replay mode with
the last visible candle mid-right-shoulder (to simulate "now" at the entry
moment) — which *hid* all the candles the trade actually plays out on. Painful
for reviewing the outcome, and it forced the visible window to double as both
the as-of cursor and the drawing-search scope.

**What changed.** New `tv-arm --start <RFC3339>` flag. When set, tv-arm treats
that timestamp as "live now" and finds the setup's drawings by searching the
**whole chart** (nearest-to-start), ignoring the visible window — so you can
leave the entire pattern *and* its future candles on screen. Per-role directional
matching (`SlotPref::NearestTo`):
- **H&S neckline** (break-and-close + retest): nearest trendline anchored
  *at-or-before* `--start`.
- **invalidation** (`too-low` / `too-high`): nearest horizontal to `--start`,
  *either side* (the cap/floor bracket the pattern).
- **M/W path**: the path whose two shoulders bracket `--start`
  (`B ≤ start ≤ D`; `start ≥ B` when the right shoulder isn't drawn).
- **trade-expiry**: nearest vertical *at-or-after* `--start`.
- **news / blackout vertical pairs**: scoped to `[--start, trade-expiry]` — a
  pair before start or after the expiry is dropped, only pairs inside the
  trade's own lifetime survive. Auto-drawn calendar bars use the same window.

`--start` also sets the prune cursor (like `--as-of`) to itself. A malformed
value is a **hard error** (unlike `--as-of`, which falls back to the cursor) —
`--start` changes discovery, so a typo must not silently revert to
visible-window matching. Absent: behaviour is unchanged, bit-for-bit.

**Scope.** tv-arm-only (arming). No engine / worker / signed-field change — the
emitted plan is identical to what you'd get by hiding the future candles and
arming normally. Intended for offline `--plan-out` journaling; on the live
`--register-plan` path it still overrides discovery + cursor if set.

**Tests.** roles: `nearest_to_picks_neckline_before_and_nearest_start`,
`nearest_to_invalidation_takes_nearest_either_side`,
`nearest_to_trade_expiry_picks_nearest_after_start`,
`nearest_to_mw_path_brackets_start`,
`nearest_to_mw_path_three_anchor_relaxes_to_after_left_shoulder`. tv-arm 177
green; clippy + fmt clean.

## Unreleased — 2026-07-01 — too-high invalidation reverted to close-confirm

**Why.** The prior "intrabar cross reads the wick" change flipped *every*
intrabar directional cross to fire on the wick, including the drawing-bound
`too-high` invalidation cap. Operator call: the invalidation cap should only
trip when a bar genuinely **closes** past it — an intrabar spike above the cap
that closes back below is not an invalidation. Reverting the cap avoids
premature invalidations on volatile shoulder-cap wicks while keeping the wick
semantics where they matter (the retest tap-and-bounce).

**What changed.** `invalidation_or_pcl_trigger`
(`tv-arm/src/trade_plan_build.rs`) now emits the short-side `too-high` cap as
`HorizontalCross { dir: Up, bar: OnClose }` (was `Intrabar`). A bar must close
above the cap to invalidate. This is a **deliberate asymmetry**: only the literal
`too-high` name reverted. The long-side invalidation **floor** (`too-low`,
`dir: Down`) stays `Intrabar` — a low wicking below the floor still invalidates
with no close required. The `too-low` pcl-exhausted veto (`CrossDir::Either`) and
the retest (`TrendlineCross`, intrabar) are untouched.

**Scope.** Pure tv-arm build-site change — no engine change (the engine's
`OnClose` arm already implements close-confirm: `prev < level && close >= level`
for `Up`). Only freshly-armed plans carry the new `OnClose`; plans already in KV
keep whatever `BarEvent` they were signed with. The continuous `entry_level_vetos`
(Bug #12) are an `is_past` predicate, independent of this cross-guard — unchanged.

**Tests.** tv-arm: `builds_hs_short_rules_with_correct_triggers` updated to
assert `OnClose`; new `ihs_long_too_low_invalidation_stays_intrabar_wick` pins
the mirror staying `Intrabar`. tv-arm 172 green; clippy + fmt clean.

## Unreleased — 2026-07-01 — cross-depth buffer (% of line price)

**Why.** With intrabar crosses now firing on the wick (previous entry), a
*shallow* graze of a retest / invalidation line counts as a cross — including a
one-tick poke that immediately retreats. On the AUD/JPY iH&S of 2026-06-29 the
neckline got three shallow retest taps (low pierced only ~0.008–0.016% past the
line) before the move that actually ran; each produced an entry that stopped
out. The unbuffered trade is **−1.43R** (SL, SL, SL, then a +1.57R runner).

**What changed.** New plan-level signed field `TradePlan.cross_buffer_pct` — a
cross-depth buffer as a **percent of the crossed level's price**. An *intrabar*
directional cross must pierce the wick at least `cross_buffer_pct%` past the
line before it counts: `Down` needs `low <= level - (pct/100)*level`, `Up` needs
`high >= level + (pct/100)*level`. `Either` (a bare straddle) and `OnClose`
(break-and-close — the close is already past the line) ignore it. Threaded
through `eval_trigger` → `level_crossed` in the shared engine, so the live worker
and `replay-candles` apply it identically. `#[serde(default)]` → `0.0`, so plans
signed before the field deserialize to the bare wick-touch behaviour unchanged.

**Default.** `DEFAULT_CROSS_BUFFER_PCT = 0.02%`, baked by `tv-arm` at arm time.
Chosen by a buffer sweep on the AUD/JPY trade: `0.0` → −1.43R; flips to **+0.57R
net** at `0.02%` (the shallow taps are filtered, leaving only the entry that runs
to TP); holds through ~0.07%; over-tightens into a starved 0-trade plan at 0.1%.
`0.02%` is the threshold where the trade turns profitable. A `tv-arm
--cross-buffer-pct` flag to override per-arm is a planned follow-up (the field +
arm-time default land now; the flag is trivial on top).

**Tests.** engine: `intrabar_down_buffer_rejects_a_graze_admits_a_real_cross`,
`intrabar_up_buffer_rejects_a_graze_admits_a_real_cross`,
`cross_buffer_ignored_by_either_and_on_close`, and the end-to-end
`plan_cross_buffer_pct_gates_the_retest` (real 6pm geometry, proves the
plan-level field threads through `evaluate_plan`). core:
`missing_cross_buffer_pct_defaults_to_zero`, `cross_buffer_pct_round_trips`.
engine 97 / core 759 / cli 57 / tv-arm 171 green; clippy + fmt clean.

## Unreleased — 2026-07-01 — intrabar cross reads the wick, not the close

**Why.** The engine's intrabar directional cross (`level_crossed`, `BarEvent::Intrabar`)
required the bar to **close** on the firing side (`Up ⇒ close ≥ level`,
`Down ⇒ close ≤ level`) on top of straddling the level. That conflated "crossed
the level intrabar" with "closed past it". A **retest** is a tap-and-bounce: on a
descending neckline the bar opens above the line, its low wicks below (a genuine
down-cross on any sub-bar timeline), then closes back above. The close-confirm
gate threw those away, so the retest prep stamped only when a bar finally *closed*
through the line — far too late. Real incident: AUD/JPY iH&S long, 2026-06-29 —
break-and-close stamped 15:00 Brisbane, three golden signal candles printed at
4/6/7pm, but the retest didn't stamp until 30 Jun 00:00 (the first close below
the line), starving every entry. The 6pm bar (O 111.582 > line 111.519,
L 111.501 < line, C 111.540 > line) had clearly crossed down — it just recovered.

**What changed.** The `Intrabar` arm now reads the wick on the cross side,
**close-agnostic**, discriminated by which side the bar *came from* (its open):

- `Down`   ⇒ `open ≥ level && low ≤ level`  (came from above, reached below)
- `Up`     ⇒ `open ≤ level && high ≥ level` (came from below, reached above)
- `Either` ⇒ any straddle (unchanged)

`BarEvent::OnClose` is **unchanged** — break-and-close still requires a real close
through the line (open one side, close the other). Only the intrabar arm moved.

**Breaking / behaviour.** This is a shared-engine change, so the live worker and
the offline `replay-candles` get it identically (no drift). It reverts the
earlier close-confirmed semantics for the **too-high invalidation** veto
(`bug12_at_entry_level_vetos` / "too-high is close-confirmed now"): too-high/too-low
and the M/W cancel/overshoot vetos are intrabar directional crosses and now fire
on the wick-from-the-open-side rather than the close. A spike that opens on the
expected side and wicks through now invalidates intrabar; a bar that merely closes
through (without crossing from the right side) no longer does. Intentional — an
intrabar cross should mean the bar genuinely traded through the level.

**Tests.** New `intrabar_down_retest_fires_on_wick_not_close` (the real AUD/JPY 6pm
geometry). Rewrote `intrabar_fires_when_bar_crosses_from_its_open_side` (was
`…_and_close_on_firing_side`) and added `intrabar_down_fires_on_open_above_low_below_regardless_of_close`
to assert the open-side/wick rule directly. Updated `pine_entry_blocked_until_retest_seen`
to block via line-above-the-bar (no straddle) rather than the old close-side
reasoning. engine 93 green, core 757, cli all green.

**Follow-up.** A tunable cross-depth buffer (must cross by N ticks / X% of the
line price) so a one-tick graze doesn't trigger a retest/invalidation — designed
next, separate change.

## Unreleased — 2026-07-01 — SL-floor renders in raw price; broker-oanda native build

**Why (SL-floor).** The SL-vs-spread floor is a pure ratio of two raw-price
distances (`sl_distance` vs `spread = ask − bid`), so the unit cancels and the
decision never depends on `pip_size`. But the operator-facing widen/reject
messages in `run_enter` divided both distances by `pip_size` to print
"34.3 pips vs 10.0 pips". On instruments whose catalog `pip_size` was wrong
(fractional-pip metals, surfaced while journaling a Silver/XAGUSD entry), that
made a *correct* decision *read* wrong, and it added a pip dependency the rule
shouldn't have.

**What changed (SL-floor).** Removed pip from the rendering entirely: the
widened-SL log, the `sl-widen-below-min-r` reject body, and its tracing line now
report `sl_distance` / widened distance / spread in **raw price**. The decision
logic (`sl_spread_floor_violation`, `widen_sl_to_spread_floor`) was already
pip-free and is unchanged. (Operator chose raw price over ticks — no new signed
field; the worker has no `tick_size` in WASM.) New test
`verdict_is_invariant_to_pip_size` documents the floor verdict is identical for
an FX-scale (0.0001) and a metal-scale (0.00001) spread with matching ratios.

**Why (broker-oanda).** The local native worker and `trade-control-broker-check`
panicked ("function not implemented on non-wasm32 targets") the moment OANDA
made its first TLS call: `broker-oanda` unconditionally enabled `oanda-client`'s
`wasm_js` feature, forcing `getrandom/wasm_js` (browser RNG) into native
binaries. TradeNation was unaffected (no `oanda-client` dep), which is why
reversals worked natively and OANDA never could.

**What changed (broker-oanda).** Moved `oanda-client { features = ["wasm_js"] }`
and `worker = "0.8"` into `[target.'cfg(target_arch = "wasm32")'.dependencies]`;
base `oanda-client` is now `default-features = false` so native getrandom uses
the OS backend. `console_error!`/`console_log!` → `tracing::error!`/`info!`
(wasm-safe). The `worker::Env` login path (`login` / `login_with_live` /
`get_secret`) is `#[cfg(target_arch = "wasm32")]`-gated; native reaches the
broker via `OandaBroker::from_api_key` (unchanged). Verified both targets build,
40 broker-oanda tests pass, `getrandom` is `default` (no `wasm_js`) on native.

**Tests.** 757 core (+1), 40 broker-oanda. wasm worker + native both build;
clippy/fmt clean.

## Unreleased — 2026-06-30 — worker: salvage a too-tight SL by widening to 11× spread

**Why.** The SL-vs-spread floor rejected every entry whose stop sat closer
than `10 × spread` to entry — including wide-spread instruments (wheat) whose
*charted* stop was a perfectly reasonable 34+ pips but still failed the floor
because the spread itself is large. A real H&S short on WHEAT_USD
(`hs-wheat-usd-7c4116b2`, 2026-06-23) had **all six** of its enter fires
blocked `rejected: sl-below-10x-spread → 0R`, so the operator entered manually.
Rejecting a too-tight stop throws away salvageable trades when the fix is simply
to give the stop more room.

**What changed.** On the worker enter path, a floor violation now triggers a
**widen-then-reject** instead of an outright reject. The stop is moved to
`11 × spread` (`SL_WIDEN_SPREAD_MULTIPLE`, strictly above the `10×` floor so it
clears with a margin), *away* from entry — never tighter. If the trade still
clears its R-floor at the wider stop (`R = tp_distance / (11 × spread) ≥
min_r`), the order is placed with the widened SL and the worker logs the widen.
If even the widened stop drops `R` below `min_r`, there is no legal stop and the
entry is rejected with the new outcome `rejected: sl-widen-below-min-r` (HTTP
422). The widen deliberately does **not** clamp to the pattern's invalidation
level — a widened stop may sit past `too-high` / `too-low`, and the continuous
at-entry level vetos handle invalidation independently (per the operator's
call).

**Behaviour.** Verified on the wheat replay: the first enter that used to 0R now
**fills and rides to break-even** (its widened stop survives the post-entry
spike), the later enters reject with `sl-widen-below-min-r` because by then price
had fallen near the TP and widening would drop `R < 1.0`. Single code path
(`run_enter`) shared by the live worker and the offline `replay-candles`
simulator, so the widen reproduces identically in both (no drift).

**Follow-up (2026-07-01): apply the widened SL to the fill/exit + display
paths, not just the entry R-check.** The first cut widened the SL inside
`run_enter` (so the live worker *places* the widened stop), but the offline
`simulate_fill` re-derived the bracket from the signed intent and never saw the
widen — so a replayed leg filled at the widened entry yet was checked against
the *old, tight* SL, stopping out at a level the live broker stop was never at.
On the wheat replay this flipped leg 1 from a (correct) break-even into a
spurious −1R. Fix: `simulate_fill` (in `engine`) now mirrors the widen exactly
as it already mirrors the entry-level-veto and spread-blackout gates — off the
fire bar's `ask_c − bid_c` — so the widened stop reaches `find_fill`, the exit
loop, and break-even arming; a `Reject` becomes `SimOutcome::Declined
{ sl-widen-below-min-r }`. The replay report's `order:` journaling line and
annotation boxes apply the same widen (shared `apply_spread_floor_widen`
helper) so the printed SL matches the protected stop instead of the un-widened
signed level. The **live worker was already correct** (it places the widened SL
and its break-even cron amends from the live broker position) — this was a
replay-only drift. 2 new engine tests (widened SL protects the leg in the fill
path; a wide spread outside the blackout window declines via the widen rather
than blacking out).

**Config.** New constant `SL_WIDEN_SPREAD_MULTIPLE = 11.0`. New pure helper
`widen_sl_to_spread_floor(entry, sl, tp, spread, min_r) -> SlWiden` and the
`SlWiden` enum, both in `core::intent`. `Resolved` gains a `min_r: f64` field
(the effective R-floor the trade was held to at resolve time) so the enter path
can re-check the floor against the widened geometry without re-resolving a
scripted `min_r` tunable.

**Tests.** 8 new unit tests on the widen helper (unchanged / degenerate-spread /
long-widen / short-widen / below-R-floor reject / min_r-override / wheat-short
salvage). 756 core tests green; workspace builds (worker wasm + CLI).

**Breaking.** `Resolved` gained a required `min_r` field — any out-of-tree
struct-literal construction must set it (all in-tree sites updated). Wire format
unchanged: nothing signed changed, the widen is a pure runtime decision, and
no intent field was added.

## Unreleased — 2026-06-30 — tv-arm: tolerate degenerate TradingView drawings

**Why.** A `tv-arm-dev` arm on NZD/CAD aborted with
`tv-mcp draw get kazo98 returned non-JSON output … invalid type: null, expected
f64`. The chart carried a stray `parallel_channel` whose third anchor read back
as `{"price": null, …}`. `Point.price` was a plain `f64`, so `serde` rejected
the **entire** drawing — and because role classification fetches every stub's
full drawing with `?`, one unrelated degenerate channel stranded the whole
setup. The channel played no role in the arm at all.

**What changed.**
- `trading-view::drawings::Point` now tolerates a `null` price (→ `NaN`
  sentinel) and a `null` time (→ `0` sentinel) via field deserializers, so the
  parse survives. `Point::is_degenerate()` flags such an anchor;
  `Drawing::has_degenerate_point()` flags a drawing carrying one.
- `tv-arm::roles::classify` skips a drawing with a degenerate anchor (a `warn`,
  then `continue`) instead of aborting. The points are **not** silently dropped
  — the H&S/fib geometry indexes anchors positionally, so dropping would shift
  the indices; declining the whole drawing is the safe choice.

**Behaviour.** A stray/half-drawn channel or fib no longer strands a legitimate
arm. Verified end-to-end against the live chart: the NZD/CAD arm now skips
`kazo98` and runs to completion (then surfaces a *real* `1 start / 2 end`
blackout-pairing chart error — the actionable message, not a crash).

**Tests.** `null_price_anchor_parses_as_degenerate`,
`null_time_anchor_is_degenerate`, `well_formed_drawing_is_not_degenerate`
(trading-view); `degenerate_drawing_is_skipped_not_fatal` (tv-arm). 174 green
across the two crates.

**Breaking.** None. `Point`'s public fields keep their `f64`/`i64` types.

## Unreleased — 2026-06-30 — amend_stop no-TP fix (demo-verified) + TN v0.11.0

**Why.** `amend_stop` on a position with **no take-profit** silently failed.
The adapter sent `existing_tp.unwrap_or(0.0)`, and TradeNation's
`AmendCloseOrder` (orderModeID 3, "Both") read `0.0` as a real take-profit at
price 0 — a closing order on the wrong side of the market — and rejected the
whole amend (`#5-9 "too close to market"`). The stop never moved. This is the
exact path the **break-even cron** (`breakeven_watch`) and the **spread-blackout
widen** (`blackout_apply`) depend on, so it was the gate before either could go
live (TEST-REPORT-amend-stop-on-open-position.md).

**What changed.**
- Upstream `tradenation-api` (tag **broker-tradenation-v0.11.0**):
  `amend_order`'s `limit_order_price` is now `Option<f64>`. `Some(tp)` →
  orderModeID 3 (both legs, unchanged). `None` → orderModeID 2 (Stop only) with
  `limitOrderPrice: "0"`, which the platform accepts and leaves the TP absent.
- `broker-tradenation-adapter::amend_stop` now passes
  `target.existing_take_profit` (the `Option<f64>`) straight through instead of
  collapsing `None` to `0.0`.
- Bumped every `tradenation-api` / `broker-tradenation` /
  `tradenation-instrument-cache` git-dep pin (root, cli, tv-arm, cron, worker,
  adapter) to **broker-tradenation-v0.11.0** (one tag → no lockfile skew).

**Verified on the experimental DEMO 2026-06-30** via the real
`TradeNationAdapter::amend_stop` (not a re-implementation):
- **With TP** — SL moved, TP preserved (Some→Some unchanged). PASS.
- **No TP** — SL moved, TP stayed absent (None→None, no phantom 0). PASS.

So the break-even stop and the blackout widen are now **clear for live** on the
amend path. (Earlier, mode 3 + `0.0` was reproduced rejected; mode 2 + `"0"`
isolated as the fix; a valid-TP control confirmed SL distance was never the
cause.)

**Breaking.** `tradenation_api::amend_order` /
`TradeNationClient::amend_order` — `limit_order_price: f64` → `Option<f64>`.

**Tests.** Upstream `amend_order_body` unit tests (mode-3 with-TP, mode-2
no-TP). Worker workspace green; clippy + fmt clean.

## Unreleased — 2026-06-30 — news-windowed reversal-close: gate on the open window + non-terminal spine

**Why.** A USD/CHF H1 H&S short (replay, `-dev` engine) had its whole plan
**erased — zero entries** by `06-close-on-reversal`. Two stacked defects:

1. **News-window gate leaked (engine).** The reversal-close is gated
   `inside_window: ["news"]`, but the engine fired it on a qualifying *long*
   reversal **9h after `news-end`** (2026-06-26 09:00, the window closed 00:00).
   Root cause: the engine processed `news-start`/`news-end` control rules but
   **discarded the open/closed state** — a news-only close was deliberately left
   to "fire on the detector match alone (the worker's news-window KV gate decides
   it)". In **replay** there is no such dispatch-time KV gate, so the permissive
   fire went through unguarded; and even live, the engine had already retired the
   spine before the worker's `run_close` could decline it.

2. **A gated-off close still retired the spine (engine).** `evaluate_guards` set
   `Phase::Done` on **every** guard fire. A news-windowed reversal-close is a
   "flatten *if* in a position" safety, not a thesis invalidation — but it tore
   the plan down (and starved every pending entry) even when it closed nothing
   (`allow_close` blocks the flatten when flat).

**What changed (engine — shared across the live worker cron and offline replay).**
- **`PlanState::open_news_windows: BTreeSet<String>`** — the engine's pure mirror
  of the worker's `news:<trade_id>:<news_id>` KV entries. `evaluate_controls`
  inserts a `news_id` on a `NewsStart` fire and removes it on `NewsEnd`. Persisted
  in the plan-state KV body, so it survives across cron ticks exactly like the
  rest of the FSM state. `advanced_vs` now counts a window open/close as an
  advance (so the recording trim doesn't drop the tick).
- **`eval_pine_guard` gates the news close on an open window.** When the close
  opted into `inside_window: ["news"]`, it may only fire while `open_news_windows`
  is non-empty — mirroring `run_close`'s `list_news_windows_for_trade` check.
  Outside the window → declined, spine untouched. (The price-window gate is
  unchanged.)
- **A news-windowed close is non-terminal for the spine** (`guard_is_terminal`).
  It fires (dispatching the flatten, which the worker / replay `allow_close`
  gates on the real position) but leaves the phase intact so pending entries
  survive. A **price-windowed** close (reversal back at the SR band) stays
  terminal — that *is* the thesis breaking.

**Behaviour.** On the repro plan the spurious 09:00 close is gone; the plan stays
in `AwaitEntry` past 09:00 and is retired the correct way (the `too-low`
pcl-exhaustion veto at 06-26 22:00, price having run 80% to TP). A news close
that *does* fire inside its window no longer starves pending entries.

**Tests.** Engine: `news_close_does_not_fire_when_no_news_window_open`,
`news_close_fires_while_a_news_window_is_open`,
`news_close_does_not_fire_after_news_end_closed_the_window`,
`news_close_is_non_terminal_for_the_spine`, `price_band_close_stays_terminal`;
plus `PlanState` round-trip / `advanced_vs` coverage for the new field. 90 engine
+ 749 core tests green.

**Follow-up.** The bug report's "expected two entries" wasn't realised — the
`too-low` veto legitimately aborts the setup at 22:00 (the move completed 80% to
TP without us). That's a *correct, separate* gate, not part of this fix.

## Unreleased — 2026-06-30 — strategy-v2 entry starvation + preps are life-of-trade (no TTL)

**Why.** A plain iH&S where BCR enters and loses −1R came back as a spurious
**0R, no entry** under `--strategy-v2` (replay trade 071, GBP/JPY H1). Two
distinct defects compounded:

1. **break-and-close re-stamp starvation (engine, shared — affects live + replay).**
   A multi-shot (`--strategy-v2`) plan stays in `Phase::AwaitEntry`, so the
   break-and-close arm re-runs every bar. `evaluate_break_and_close` had **no
   latch guard**, so each later neckline re-cross re-stamped `break_close_at`
   forward — pushing the already-seen retest *before* the new break window, so
   `retest_satisfied` flipped false and the stop enter was starved forever
   (re-prepping in place; the prep fired 4× despite `FireMode::Once`). The
   reported "QM verdict clobbers stop verdict" aggregation theory was wrong; the
   stop leg never reached `push_fire` because the retest gate re-closed under it.

2. **missing-prep on every preps-gated enter (replay-only regression + a latent
   wall-clock-TTL trap).** Commit `1c0a043` routed replay enters through the real
   `run_enter` (whose prep gate is store-backed: `store.get_prep`), but the
   replay loop never seeded store preps (`Action::Prep` fell into `_ => {}`).
   Even once seeded, prep TTLs were computed `ttl_hours_until(now, trade_expiry)`
   — wall-clock-relative — so a `--plan-out` plan armed against a *historical*
   window (whose `trade_expiry` is already past relative to `now`) collapsed the
   TTL to the 1h floor, ageing the break-and-close prep out ~12h before the
   entry bar. Live (future `trade_expiry`) was unaffected, so this was a
   replay-fidelity bug, not a live one.

**What changed.**
- **Engine (`engine/src/evaluate.rs`):**
  - `evaluate_break_and_close` honours its `FireMode::Once` latch (skip if already
    in `state.fired`), so a re-cross can't walk `break_close_at` forward.
  - `stamp_retest` now **emits the `04-prep-retest` fire** the bar it stamps (and
    only once), seeding the store the enter's prep gate reads — exactly as the TV
    `04-prep-retest` alert did, and as `evaluate_break_and_close` already emitted
    the break-and-close prep. The engine still validates retest internally via
    `retest_satisfied`; the fire is what makes the store-backed `run_enter` gate
    agree.
- **CLI (`cli/src/trade_patterns.rs`):** the structural trade preps
  (`break-and-close`, `retest`) are now **no-TTL** (`PREP_NO_EXPIRY_HOURS`, ~100y,
  mirroring `core::state::NO_TTL_SECONDS`). A prep is a milestone the trade has
  passed — it lives for the life of the trade and only has to happen once; the
  trade is retired by its own `trade-expiry` veto, not by a prep ageing out.
  Drops the now-unused `now` param from both prep builders.
- **Replay (`cli/src/bin/replay_candles/replay.rs`):** the per-tick dispatch loop
  routes `Action::Prep` fires through `handle_prep` (→ `set_prep`), mirroring the
  worker's `dispatch_action` so the store the offline `run_enter` reads is seeded
  identically.

**Result.** `--strategy-v2` now fires `05-enter` exactly once and matches BCR
fire-for-fire: both enter at 06-26 23:00 BNE and stop out at **−1R** (was a
spurious 0R for v2). Verified on the live GBP/JPY chart, before/after.

**Tests.** New `break_and_close_does_not_restamp_after_latching_under_multi_enter`
(engine). Updated `entry_blocked_until_retest_then_fires_and_done` and
`stop_enter_fires_only_after_break_and_close_then_retest` to expect the retest
prep fire ahead of the enter(s). 587 cli+engine+tv-arm tests + 749 core green.

**Follow-up.** Bug report `BUG-replay-strategy-v2-entry-starved-reprepping.md`
can be closed; its suggested per-leg aggregation fix was not the actual cause.

## Unreleased — 2026-06-28 — Spread-blackout moved to `core`; replay applies it

**Why.** The spread-blackout entry reject (System 1) lived in
`src/spread_blackout.rs` — inside the worker crate, a `cdylib` the
`engine`/`cli` crates cannot depend on. So the offline candle replay
(`replay-candles` → `engine::simulate_fill`) could not apply it and **silently
filled entries the live worker would reject** (a 423), making the replay an
unfaithful production dry-run. The replay candles carry real per-bar bid/ask
books, so the fire-bar spread is in the data — the reject is fully
reconstructable offline. This is item 1 of `REPLAY-PARITY-AUDIT.md` and follows
the established `pause_gate` / `retry_gate` / `Breakeven` shape: pure decision in
`core`, thin I/O wrapper in the worker
(`[[strategy_changes_in_both_replayer_and_worker]]`).

**What changed.**

- **`core::spread_blackout` (new, shared):** the pure `spread_blackout_decision`,
  `elevated_threshold_pips`, `baked_baseline`, the `SPREAD_REJECT_MULTIPLE` /
  `SPREAD_BLACKOUT_ELEVATED_PIPS` / `SPREAD_BLACKOUT_RECOVERED_PIPS` constants,
  and the baked-baseline module moved here verbatim (with their unit tests). The
  compile-time baseline bake moved with them: **`core/build.rs`** reads the
  `spread-sampler-cron` YAML samples (path adjusted to `../../` for core's depth)
  and emits the per-instrument table; a missing samples dir still bakes an empty
  table (flat-8 fallback), unchanged.
- **Worker (byte-identical behaviour):** the old `src/spread_blackout.rs` is now a
  thin re-export shim over `trade_control_core::spread_blackout`. Every
  `crate::spread_blackout::*` call site (`run_enter`, `blackout_watch`,
  `blackout_cancel`) is unchanged. The now-dead root `build.rs` and its
  `[build-dependencies]` were removed (the bake lives in `core` now).
- **Replay (`engine::simulate_fill`):** after resolving the entry and the
  at-entry level veto, the simulator computes the **fire bar**'s spread
  (`(ask_c − bid_c) / pip_size`) and calls the shared
  `core::spread_blackout::spread_blackout_decision`. The `window_open` input is
  modelled offline by `core::ny_clock::is_ny_close_edge(fire_bar.time)` (the live
  KV window marker has no offline equivalent — documented as an approximation). A
  reject returns the new `SimOutcome::SpreadBlackout { spread_pips,
  threshold_pips }`, mirrored to `FillOutcome::SpreadBlackout` (fixture), a
  `FillKind::SpreadBlackout` (report/annotate, a not-taken kind), and a report
  line: `spread: REJECTED — spread 30.0p > 8.0p threshold inside the
  NY-close-edge window (no order placed; live worker 423s)`. A mid-only feed
  (`bid == ask`) has zero spread → never blacks out.

**Breaking.** New `SimOutcome::SpreadBlackout` and `FillOutcome::SpreadBlackout`
variants (matched exhaustively across `report.rs` / `fixture.rs` /
`replay_broker.rs` / `annotate.rs`). No signed-wire or KV change. Worker
behaviour identical — this is a refactor + replay-fidelity addition, **not** a
worker behaviour change, so no redeploy is required.

**Config.** None. Thresholds, the baked baseline, and the re-bake cadence are
unchanged (the table is byte-identical — same samples, same extraction, now run
from `core/build.rs`).

**Tests.** Pure-decision unit tests moved to `core` (11 incl. the baked-baseline
self-consistency + 5×-median checks over the 1240-sample table). New
`simulate_fill` tests: elevated spread inside the NY-close-edge window →
`SpreadBlackout`; tight spread inside the window → fills (passes the gate); wide
spread *outside* the window → fills; mid-only zero-spread bar → never blacks out.
`fixture.rs` round-trip extended to the new variant. Existing
`all_fixtures_match_expected` regression still passes (no fixture churn — no saved
fixture exercises the new path).

**Follow-up.** The `is_ny_close_edge` window stand-in is exactly the close-edge
hour, whereas the live KV window can persist a little longer until the recovery
watcher clears it — a small known optimism. Items 2–3 of the parity audit (order
sweep breach predicate, market-hours blackout) are the next moves.

## Unreleased — 2026-06-28 — Engine fires `06-close-on-reversal`; replay honours it

**Why.** A `06-close-on-reversal` rule (a `PinePattern` close bound to the
*opposite* direction, gated on price ∈ `sr_bands`) **never fired** in the
cron-engine era — neither in the live worker nor in the offline `replay-candles`
simulator. Root cause: `engine::evaluate::eval_trigger` returns `false` for
every `PinePattern` trigger, and Pine detection was wired only into the *entry*
path (`eval_pine_entry`), never the guard path. So a guard carrying a
`PinePattern` (the close) was inert. A trade that should have exited on a
confirming reversal candle over-held to SL / TP / window-end. (The close *was*
honoured in the now-retired TV-alert era, where TradingView's Pine study fired
the alert and the worker only gated it; the engine port never replaced that
trigger half.) Caught replaying demo trade 075 (Wheat H1 H&S short, leg 3):
`88 pine-enter` evaluations, `0 pine-close`.

**What changed.**

- **Engine (shared — fixes worker dispatch *and* replay):** a `PinePattern`
  guard is now routed through the candle detector (`eval_pine_guard`, sharing
  `eval_pine_entry`'s direction/kind/fires match) instead of `eval_trigger`. On
  a detector match it applies the **pure** half of `run_close`'s contextual gate
  — when the intent lists `inside_window: price`, the reversal candle's close
  must sit inside one of `sr_bands` (new pure helper `price_in_any_band`, mirror
  of the worker's `price_band_hit`). A news-only close has no recomputable price
  gate here and fires on the detector match alone (the worker's news-window KV
  gate decides it at dispatch). On fire it pushes the intent **with the latched
  signal** onto the shell (so `run_close` sees `golden`/`confirmed`) and retires
  the plan (`Phase::Done`), like any terminal guard.
- **Replay (`replay-candles`):** the pure per-enter `simulate_fill` knows only
  the bracket, so a reversal-close fire is applied as a post-pass
  (`report::apply_reversal_close`): an open position is flattened on the earliest
  reversal-close that lands while it's open, before its SL/TP. New
  `FillKind::ClosedOnReversal`; the report prints `fill: CLOSED ON REVERSAL — in
  @ <entry> → exit <price> (<bar>)` and tallies `REV:` distinctly from `TP:` /
  `SL:`; `--annotate` draws it (outcome label `reversal`).

**Behaviour change (live).** When `staging` (or any env) is next deployed, the
worker will begin auto-closing positions on `06-close-on-reversal` fires — the
intended behaviour, previously dormant. No effect until a deploy; coding stays
on `main`/dev.

**Breaking.** `report::resolve_fire` / `resolve_fire_any` now take an extra
`closes: &[CloseFire]` argument (replay-internal; not a wire/serde change).

**Tests.** Engine: a long reversal in the band fires the close; outside every
band declines (plan stays alive); news-only fires on the detector alone;
same-direction reversal is ignored; `price_in_any_band` endpoints. Replay:
`apply_reversal_close` unit matrix (before-fill ignored, after-SL moot,
before-SL pre-empts, earliest-wins, never-filled untouched) + an end-to-end
`run`→`render` test (a multi-shot short fills, a bullish reversal in the band
prints, the short reports `CLOSED ON REVERSAL` / `REV: 1`, not "still open").

**Follow-up.** The replay flattens at the reversal **bar close** (the worker
flattens at market on that tick — a faithful proxy with only OHLC). A
spread-aware exit price would need the bid/ask book the worker actually trades.

## Unreleased — 2026-06-28 — Break-even stop at 50%-to-TP

**Why.** There was no stop-management rule of any kind: after entry the SL
stayed at its original level for the life of the position, so a leg that ran
≥50% to TP and then reversed took a full −1R instead of a 0R scratch (demo
trade 075 Wheat H&S short, leg 2 — `BUG-replay-no-breakeven-stop-at-50pct`).
The standing lesson ("once profit reaches 50%, set SL to break-even") was not
encoded in the engine.

**What changed.** A break-even rule, baked as signed data on the enter intent
and applied identically by both consumers (replay + live worker) via a pure
core helper so they can't drift:

- **core** `intent::Breakeven { threshold }` — pure helpers: `arms_at(entry,tp)`
  (directional 50%-to-TP level), `close_arms` (inclusive **close-past** test —
  a wick does not arm), `target_stop` (= entry, a 0R scratch), `decide_move`
  (the worker's per-tick decision; idempotent & one-way), `more_progressed`.
  Degenerate thresholds clamp to 0.5.
- **replay** `engine::simulate_fill` — the exit walk tracks a latched
  `active_stop`; once a candle CLOSES past the 50% level the stop moves to
  entry for subsequent bars. Report shows `BREAK-EVEN (SL→BE)` when the stop
  landed at entry.
- **live worker** `src/cron/breakeven_watch.rs` — new every-tick cron: joins
  open positions to their `EntryAttempt`'s `BreakevenSnapshot`, fetches closed
  candles at the trade granularity, and `amend_stop(entry)` once a close passes
  50%. Shares `blackout_apply`'s demo-confirm precondition on
  `AmendCloseOrder`-on-open-position (every intended amend logged first).
- **arm** `05-enter` carries the rule **by default at 50%** (H&S, the
  strategy-v2 QM leg, and M/W). tv-arm `--no-breakeven` opts out;
  `--breakeven-pct <f>` overrides. Spec field `breakeven_pct` (`null` disables).

**Breaking.** `Intent.breakeven: Option<Breakeven>` and `Resolved.breakeven`
added (serde default-absent — in-flight intents/plans unchanged).
`EntryAttempt.breakeven: Option<BreakevenSnapshot>` added (serde default-absent).
`retry_gate::record_placement` gains a `breakeven` param; `run_enter` gains an
`enter_granularity` param (engine passes the plan granularity; webhook +
blackout-restore pass `None`).

**Config.** `TradeSpec.breakeven_pct` (default `Some(0.5)`; `null` disables).
tv-arm `--no-breakeven` / `--breakeven-pct`.

**Tests.** Core (`Breakeven` helpers incl. trade-075 leg-2 geometry +
`decide_move`/`more_progressed`), engine (`simulate_fill` −1R→0R; wick does not
arm), CLI (default 50% baked on the enter only; `null` disables; custom carried),
worker cron (join). Full workspace green; clippy + fmt clean.

**Follow-up.** Tag + deploy **deferred** — `staging` is mid-bake on v60 (the
week-long promotion gate). ~~Demo-confirm `amend_stop` on an open TN position
before trusting the live move~~ — **DONE 2026-06-30** (see the amend_stop no-TP
fix entry at the top): `amend_stop` is now demo-verified on the experimental
account for both the with-TP and no-TP cases, after fixing a no-TP rejection
bug this test surfaced. Shared precondition with the blackout widen is
satisfied. The operator's stance ("the SL is handled by the broker") means BE
only needs to arm once and set the broker-native stop.
## v61 — 2026-06-28 — ATR-based entry/SL buffer (`offset_atr_pct`)

**Why.** The entry and stop-loss were offset from the signal candle by a
hardcoded ±1 pip, which doesn't scale with instrument volatility — a buffer
that's safe on EUR/USD is nothing on a noisy instrument like Wheat (demo trade
075). The buffer should be a function of recent volatility so a quiet pair gets
a tight buffer and a noisy one a proportionally wider one, with no per-instrument
hand-tuning.

**What changed.** Every anchored offset (entry trigger, `stop_loss`, anchored
`take_profit`) now accepts **`offset_atr_pct`** — a buffer as a percent of ATR,
resolved at fill time as `(offset_atr_pct / 100) × shell.atr`. The ATR is the
Wilder ATR the signal detector already latches per bar (per-timeframe length),
recomputed each tick from the candle window the engine holds — no new ATR
plumbing, no broker pull. New H&S / iH&S enters (CLI `build-trade` + `tv-arm`)
default to `offset_atr_pct: 0.5` on entry and SL, replacing the ±1-pip default.
`offset_atr_pct` is an unsigned magnitude (direction comes from the anchor).
Resolution is shared in `trade_control_core`, so the live worker and the offline
replay can't drift. The engine simulator honours it automatically (same pure
`Resolved::from_intent`).

**Behaviour.** Fail-closed: an `offset_atr_pct` offset on a bar with no ATR
(warmup / short feed) declines that bar (the plan stays armed and retries next
tick) rather than placing a zero-buffer trade. `offset_atr_pct` and
`offset_pips` are mutually exclusive; `offset_atr_pct` on a `close` anchor is
rejected; a negative `offset_atr_pct` is rejected.

**Breaking.** `PriceRef::resolve` / `resolve_tp` are now fallible
(`Result<_, OffsetError>`). `IntentValidationError` drops its `Eq` derive (the
new `OffsetSpecInvalid(OffsetError)` variant carries an f64). `TradeSpec` gains
`entry_offset_atr_pct` / `sl_offset_atr_pct` override fields.

**Config.** New enter-intent offset field `offset_atr_pct` on
`PriceRef::Anchored` / `EntrySpec::{Stop,Limit}` (serde-default, skip-if-none →
old wire byte-identical). `offset_pips` is **deprecated** but still honoured for
in-flight / hand-armed plans and explicit pip buffers. CLI interactive prompts
now offer ATR-pct (default) or pips for directional anchors.

**Tests.** New core resolution tests (buffer pushes away / scales with vol /
no-ATR rejects / both-set rejects / close-anchor rejects / negative rejects /
pips path unchanged); 4 parse-time `validate` tests; 2 engine simulator parity
tests (buffered fill + no-ATR Unresolved). Full workspace green.

**Follow-up.** None required — M/W uses its own baked-price geometry and is
untouched; `entry_level_vetos` bake absolute prices and are unaffected.

## v60 — 2026-06-27 — Staging bake marker (no code change)

Marker release pinning the staging promotion candidate for its week-long bake
starting Monday 2026-06-29. **No code change since v59** — this tag exists so
the exact commit that runs unchanged on `staging` for the week (the promotion
gate) is unambiguously identified before a `prod` promotion.

- Code is byte-identical to v59 (the bug #15 no-TTL + control-event + purge ship).
- Secrets backfilled onto the suffixed `-dev` / `-staging` workers this session
  (post-2026-06-22 env rename): `ADMIN_KEY`, regenerated `OANDA_API_KEY`,
  `OANDA_ACCOUNT_ID`, `OANDA_ACCOUNT_OANDA_REVERSALS_DEMO`, and the two
  TradeNation demo creds (`TN_ACCOUNT_EXPERIMENTAL` / `TN_ACCOUNT_REVERSALS`).
  Cloudflare Worker secrets do not expire — this was a one-time backfill, not a
  recurring thing. No repo change (secret-store ops only).
- **Promotion plan:** if `staging` runs unchanged + profitable for the week, it
  merges into a fresh `prod` branch (worker `trade-control-web-hook-prod`).

## v59 — 2026-06-27 — No-TTL per-trade rows + control-event trail + purge commands (bug #15)

### Why

The `plan-state:` KV row (the engine's per-trade watermark + FSM state) was
written with a flat ~1-day TTL. When it aged out while the plan was still live,
the next cron tick read `None`, **re-seeded** (`tick_one → seed_first_tick →
seed_plan_state`), jumped the watermark to the newest candle, and fired nothing
— silently skipping any **price-cross veto** in the gap (the wall-clock
`trade-expiry` survives a re-seed; a price-cross veto does not). This was the
unfixed half of the 2026-06-23 TTL fix (775092e), which de-TTL'd `plan:` /
`archived-plan:` but left `plan-state:` on a flat TTL — so the state row
outlived its plan by only ~1 day while the plan now lives forever. Reproduced on
the GBP/USD inverse-H&S twins: `reversals` fired `01-veto-too-low` correctly;
`experimental` re-seeded past the 15:00 cross and fired only
`02-veto-trade-expiry` (**bug #15**).

Fixing that pushed the whole per-trade-row model to no-TTL, which in turn needs
explicit cleanup (TTL no longer reclaims the rows) and a way to review a TTL'd
control's lifecycle after it passively vanishes — hence the control-event trail
and the purge commands.

### What changed

Two classes of KV row now have deliberately different lifetimes:

- **Per-trade lifecycle rows are no-TTL** — `plan:`, `plan-state:`,
  `archived-plan:`, `entry-attempt:` (+ each attempt's `order-body:`), and the
  new `control-event:`. They live until an explicit `plan purge`. This is what
  fixes bug #15: the plan and its state row can no longer fall out of sync by
  expiry.
- **Control / dedup rows keep their window-anchored TTL** — `cooldown:`,
  `veto:`, `prep:`, `pause:`, `news:`, `spread-blackout:`, `blackout-hours:`,
  `seen:`, `retry-fire-seen:`. Expiry *is* their intended behaviour (a cooldown
  lapsing is the cooldown ending).

- **`put_plan_state` no longer TTLs** (drops `plan_ttl(now + 1 day)`), mirroring
  `put_trade_plan` / `archive_plan`. `read_plan_state_settled` re-reads once on a
  `None` before committing to a seed, so a transient KV eventual-consistency
  read-miss can't trigger a watermark-jumping re-seed either; it logs loudly when
  a re-seed/recovery happens.
- **Entry-attempt + order-body rows are no-TTL too.**
  `record_entry_attempt` / `set_entry_attempt_broker_trade_id` /
  `put_order_body` drop their `.expiration_ttl`. The retry gate's cap
  (`attempts.len() >= max_retries`) is scoped to one `(account, trade_id)`, so a
  non-expiring attempt from a finished trade never affects a new trade's count.
- **Durable control-event audit trail.** KV deletes a TTL'd control row passively
  when its window lapses — no event, no trace. A new no-TTL
  `control-event:{scope}:{trade_id}:{suffix}` row is written on **every** per-trade
  TTL'd control set (cooldown / veto / prep / pause / news), capturing
  kind/name/instrument/set_at/ttl_seconds/computed_expiry/request_id
  (`computed_expiry = set_at + ttl`, the best available "and it lifted at…"). It
  lets you reconstruct a control's set→expire lifecycle when journaling a past
  trade. Append-only, read back via `list_control_events`, dropped by `plan
  purge`. Global / instrument-only sets (spread-blackout, blackout-hours) aren't
  per-trade and carry no trail.
- **Purge commands.** `trade-control plan purge <trade_id>` is a superset of
  `plan delete`: it deletes every per-trade KV row (plan / state / archived /
  entry-attempt / order-body / control-event + enumerable trade-scoped pause /
  news) **and** the trade's R2 `ticks/` bundles; window-TTL'd `veto:` / `prep:`
  rows are left to self-clear (their lifecycle is in the control-event trail).
  `trade-control purge --older-than <days>` is a bulk R2 retention sweep over
  `req/` + `ticks/` by date prefix (KV untouched). New `src/r2_purge.rs`
  (`purge_trade_ticks` / `purge_older_than`, fail-soft) with pure `key_date` /
  `key_is_for_trade` helpers unit-tested (incl. the hs-1/hs-10 prefix-collision
  case).
- **R2 is now no-TTL** — recording bundles persist until a purge command removes
  them.

### Breaking

- `StateStore` trait surface changes (all impls — KV, MemStore, retry-gate +
  worker-test fakes — updated):
  - **`put_plan_state` drops its `ttl_seconds` param** (now no-TTL). The
    `plan_ttl` helper + its tests and the unused `expires_at` on
    `persist_plan_state` are removed.
  - **`put_order_body` drops its `ttl_seconds` param** (now no-TTL).
  - New methods `record_control_event` / `list_control_events` /
    `clear_control_events`.
- New actions **`Action::PlanPurge`** (requires `trade_id`) and
  **`Action::PurgeOlderThan`** (carries its day-count cutoff in `not_before`).
- New `core::control_event::{ControlEvent, ControlKind}`.

### Config

None. No new secrets or `wrangler.toml` bindings — purge reuses the existing
`TRADE_CONTROL_KV` + `TRADE_CONTROL_R2` bindings. (Operational note: with R2 and
per-trade KV rows no longer TTL'd, growth is now bounded by running the purge
commands rather than by TTL.)

### Tests

- `engine/tests/bug015_repro.rs` (3): the FSM fires `too-low` correctly against
  the real TradeNation feed in every batching scenario (proving the engine was
  never the bug), and a re-seed-after-cross reproduces the experimental twin's
  exact `fired=[trade-expiry]` terminal state.
- `memstore_plan_state_round_trips` now asserts the row's expiry is far-future
  (no TTL).
- `control_event` module (4) + `memstore_control_events_append_list_and_clear`
  (ordering, no-TTL, trade scoping, clear).
- `src/r2_purge.rs`: 8 tests incl. the hs-1/hs-10 prefix-collision case.
- Full workspace green (core 605, engine 59+3 repro, worker 217 incl. 8
  r2_purge, cli 253).

### Follow-up

The control-event trail covers per-trade TTL'd sets; global / instrument-only
sets (spread-blackout, blackout-hours) still leave no trace when they lapse —
add a trail for those only if a journaling need surfaces. The downstream
`trade-analyzer` R2 consumer may want to read the `control-event:` data when
reconstructing a trade's timeline.

## v58 — 2026-06-25 — Replay: cancel-and-replace a resting sibling order; no overlapping positions

### Why

A `--strategy-v2` replay (XLM/USD, two enters — `05-enter` stop +
`09-enter-qm` limit, both `max_retries: 5`) reported **three overlapping
positions** the live worker would never have taken:

1. A new entry firing while a prior order was still **resting** did not
   cancel that resting order — the replay let both rest and fill
   (cancel-and-replace was missing). *(Bug 1)*
2. With the resting order still alive, a later entry's position **stacked
   on top of** the prior one — two open positions at once. *(Bug 2)*

The worker was always correct here: the shared retry gate
(`core::retry_gate::evaluate`) asks the **broker** whether a prior attempt
is resting and, on `Pending`, cancels-and-replaces it; on an open position
it blocks. This was a **replay-only** fidelity gap — the offline
`ReplayBroker` mis-reported a still-resting order as `Cancelled` (a free
slot) instead of `Pending`, so the gate never took its cancel path, and
the report then re-simulated each enter fire in isolation with no
cross-fire awareness.

### What changed

- **`ReplayBroker.resolve()`**: a not-yet-filled (still-resting) order now
  resolves to `AttemptState::Pending`, exactly what the real broker reports
  — so the shared gate cancels-and-replaces it for a sibling/re-entry.
  (Declined/unresolved stay `Cancelled`; a genuinely cancelled attempt is
  still caught by the `cancelled` flag.)
- **Cross-fire propagation**: `run()` stamps `Fire.superseded` on any
  recorded enter whose resting order the gate later cancelled (correlated by
  `order_id`). The report shows it as `SUPERSEDED — resting order cancelled
  by a later entry`, not a fabricated standalone fill; `--annotate` no longer
  draws it as a taken position.

### Breaking

None (replay-only; `Fire` gains `order_id` / `superseded` fields).

### Tests

- New `a_new_enter_cancels_a_resting_sibling_order_no_overlap` (replay.rs):
  two-enter multi-shot plan; the resting stop is superseded by the later
  limit, report shows `SUPERSEDED` and tallies one TP / no overlap.
- Updated `open_then_closed_as_the_asof_bar_advances` (replay_broker.rs):
  as-of the fire bar, a resting order is now `Pending` (was `Cancelled`).

### Follow-up

The decision logic is unchanged and shared; this only aligns the replay's
broker-state reporting + journaling with what the worker already does. See
the `strategy-changes-in-both-replayer-and-worker` memory.

## v57 — 2026-06-24 — Recover wrong-side stop entries (rename `on_too_close` → `recover_entry`)

### Why

An H&S / iH&S short enters on a stop at `signal_low − 1pip`. When price
breaks **down through that trigger during the 2-bar signal-confirmation
wait**, the stop is "wrong-side" by the time the signal confirms
(`trigger ≥ close` for a short). The resolver returned `InvalidGeometry`,
the engine treated that as "decline this bar", and the trade was
**silently dropped** — even when the thesis was right and price ran to TP.
Proven on Euro Stocks 50 (2026-06-23): trigger 6306.2, confirm-bar close
6302.3, SL 6329.0, TP 6210.65 → a confirmed setup abandoned, ~3.4R left on
the table. The recovery machinery already existed for the broker `#19-10`
case, but never fired at resolve time and was misleadingly named.

### What changed

- **Resolver recovers a wrong-side stop instead of dropping it** — *when
  opted in*. `recover_entry: { action: market }` re-keys to a market entry
  at the current close; `{ action: limit }` rests a limit at the original
  trigger (correct-side re-checked) for the pullback, preserving R. `skip`
  / absent keep today's drop (zero blast radius for un-opted stops). The
  recovered entry flows through the same resolver tail, so the **≥1R floor**
  and the **in-range** check re-run against the new reference; the worker's
  **SL≥10×spread** floor still applies.
- **Derived slippage default.** A `market` recovery no longer *requires*
  `max_slippage_pips`; when omitted the resolver derives the bound from the
  SL→entry distance (`|stop_loss − trigger|`). Explicit pips still win.
- **tv-arm `--recover-entry market|limit|abort`** (H&S / iH&S only;
  `abort → skip`). When omitted the default is keyed off
  `--require-confirmation`: a confirmation-required setup (whose lag is what
  strands the stop) defaults to **`limit`**; otherwise **drop**. M/W is out
  of scope (no `EntrySpec`).

### Breaking

- **Concept rename `on_too_close` → `recover_entry`** across the wire and
  the codebase: struct `OnTooClose` → `RecoverEntry`, enum
  `OnTooCloseAction` → `RecoverEntryAction` (variants `Market`/`Limit`/`Skip`
  unchanged), `ResolvedOnTooClose` → `ResolvedRecoverEntry`,
  `Resolved::on_too_close`/`EntrySpec::Stop::on_too_close` →
  `recover_entry`, `IntentValidationError::OnTooCloseMissingSlippage`
  removed, `src/too_close.rs` → `src/recover_entry.rs`
  (`market_replace_plan`/`TooClosePlan` → `recover_entry_plan`/
  `RecoverEntryPlan`). `EntryError::EntryTooCloseToMarket` (the broker
  `#19-10` condition) is **unchanged**.
- **Wire back-compat:** `#[serde(alias = "on_too_close")]` on the renamed
  `EntrySpec::Stop::recover_entry` field, so in-flight signed KV plans
  still parse. Action values (`market`/`limit`/`skip`) are unchanged.

### Config

- `entry.recover_entry: { action: market|limit|skip, max_slippage_pips?: f64 }`
  on a `stop` entry (was `on_too_close`). `max_slippage_pips` is now optional
  for `market`. CLI: `tv-arm --recover-entry market|limit|abort`.

### Telemetry (breaking for log greps)

- Recovery skip-reason strings `too-close-*` → `recover-entry-*` (e.g.
  `too-close-limit-wrong-side` → `recover-entry-limit-wrong-side`,
  `too-close-no-fallback` → `recover-entry-none`). The engine
  rejected-entry-spec tracing field `on_too_close=` → `recover_entry=`. The
  broker-failure outcome string `entry-failed: too-close-to-market` is
  **unchanged** (it names the broker condition).

### Tests

- Core: 8 new resolver tests (bare/skip drop; market@close + derived/
  explicit slippage; limit@trigger; below-min-R refused; past-TP
  out-of-range; the exact Euro fixture → Limit@6306.2 SL 6329.0 TP 6210.65);
  serde alias round-trip; validation now *accepts* a bound-less market. CLI:
  threading (Skip→no field, Limit→Some+validates, Market entry ignores it).
  tv-arm: flag mapping + Option default. core 593 / engine 53 / worker 211 /
  cli 249 / tv-arm 142 — all green.
- **Replay proof** (`/tmp/euro.json`): bare → resolve-failed → drops →
  `too-low` veto; `recover_entry:{action:limit}` → "dispatchable — will
  fire enter", SHORT limit @ 6306.2 → TOOK PROFIT (TP:1 SL:0);
  `{action:market}` → SHORT market @ 6302.3 → TOOK PROFIT.

### Follow-up

- The broker-`#19-10` `place_entry_too_close_fallback` (`src/lib.rs`) still
  re-places via `broker.place_entry` directly, bypassing the SL≥10×spread
  and R-floor worker gates. Documented, not fixed here.
- Not deployed — landed + verified on `main`/dev via replay only.

## v56 — 2026-06-23 — OANDA practice-vs-live is per-account (not a global secret)

### Why

The worker picked the OANDA host (practice `api-fxpractice` vs live
`api-fxtrade`) from one worker-global `OANDA_LIVE` secret, read inside
`broker-oanda`'s login regardless of which named account was trading. That
makes it impossible to run a demo and a live OANDA account in the same worker,
and it silently couples a named demo account's host to a global flag the
operator may have set for something else.

### What changed

- **Named OANDA accounts derive practice-vs-live from their own `kind`.**
  `acquire_oanda_broker_for_account` now passes `meta.kind.is_live()` down:
  `demo` → practice host, `live` → live host. The global `OANDA_LIVE` secret is
  bypassed entirely for the named-account path.
- `broker_oanda::login_with_account_id(env, account_id, live)` gained the
  explicit `live` flag; `oanda::login_with_live(env, live)` is the new
  host-from-flag constructor. The pure `live_flag_from_secret` helper isolates
  the `OANDA_LIVE` string parsing.
- **Legacy global path unchanged:** `login(env)` (intents with no `account:`)
  still reads `OANDA_LIVE`.

### Breaking

- `broker_oanda::login_with_account_id` now takes a third `live: bool` argument.

### Tests

- `live_flag_tests` in `broker-oanda/src/oanda.rs` cover the `OANDA_LIVE` parse
  (absent → practice, case-insensitive `true` → live, everything else →
  practice). Existing `AccountKind::is_live` tests cover the per-account flag.

### Follow-up

- None. The legacy `OANDA_LIVE` secret can stay for the global path; named
  accounts no longer consult it.

## v55 — 2026-06-22 — replay-candles: pull one bar past the trade-expiry so it actually fires

### Why

After v54, a bare `replay-candles --plan plan.json` resolved its window end to
the plan's trade-expiry — but the replay then reported `0 fires` / `Done: false`
even on a plan whose trade-expiry clearly should have retired it (NZD/CHF M15,
expiry 19:30 BNE). The engine evaluates a `TimeReached` (trade-expiry) trigger
against each candle's **open** time (`candle.time >= at_epoch`, `evaluate.rs`).
With the window ending *exactly at* the expiry, the last bar *opened* one bar
short of it (e.g. opens 19:15, expiry 19:30), so no candle ever satisfied the
predicate and the expiry never fired.

### What changed

- **Pull one granularity bar past the window end** (`pull_end = end + 1 bar`) so
  a candle that *opens at* the trade-expiry is fetched and evaluated. The
  displayed/`expires_at` `end` is unchanged; only the candle-pull range extends.
  Harmless when there's no expiry — the engine stops at the first `done` and
  ignores trailing candles. The extra bar is logged as `pull_end`.
- **Replay `now` is the bar's close time** (`candle.time + granularity`) instead
  of its open time, so wall-clock-derived state (TTLs, logging) matches the live
  worker, which ticks on wall-clock rather than bar-open. (Note: this does *not*
  affect `TimeReached`, which the engine keys off `candle.time` directly — the
  window-extension above is what fixes the expiry firing.)

### Breaking

None. CLI behaviour fix only; no wire-format / KV / signed-field change.

### Tests

`trade-control-cli` (native): a trade-expiry whose epoch a bar opens at fires +
finishes the plan; the converse (window a bar short → no fire) confirms the
need for the extension. Existing replay tests updated for the new `run`
signature (now takes the granularity).

### Verified

Re-ran the NZD/CHF M15 plan (`hs-nzd-chf-9457e0d7`): now reports
`02-veto-trade-expiry Veto @ 19:30` / `Done: true` / `1 fire`, where before it
was `0 fires` / `Done: false`.

## v54 — 2026-06-22 — replay-candles: window from the replay cursor + the plan, not the visible region

### Why

The TradingView-defaults workflow (v47) read the *whole visible region* as the
replay window and the granularity off the chart resolution. In practice the
operator's natural move is to put TradingView in **replay mode at the start of
the trade** and run `replay-candles` — at which point the chart only renders
bars up to the replay cursor, so the *last shown candle* is the trade start, not
some scrubbed-to right edge. And the granularity is already pinned by the signed
plan, so reading it off the chart was a redundant source of mismatch errors.

### What changed

- **start = the chart's last shown candle** (`bars_range.to`, the replay
  cursor) instead of `visible_range.from`. `--start` still overrides.
- **end = the plan's trade-expiry** — the `TimeReached.at_epoch` of the rule
  whose `rule_id` contains `trade-expiry` (e.g. `02-veto-trade-expiry`, the same
  id the engine keys on). Falls back to the chart's visible-region end
  (`visible_range.to`) when the plan has no such rule. `--end` still overrides.
- **granularity comes from the plan** (`plan.granularity`), no longer read from
  the chart resolution. `--granularity` is now an *override only*, and an
  override must still match the plan's granularity (else it's refused).
- TradingView is consulted only when something it provides is actually needed
  (the start cursor, the symbol, or the end-fallback when the plan has no
  expiry). A run whose end comes from the plan and whose start/instrument are
  flagged makes no MCP call.

### Breaking

None to the wire format. CLI behaviour change: a bare
`replay-candles --plan plan.json` now replays `[last-shown-candle,
plan-trade-expiry]` instead of `[visible.from, visible.to]`, and `--granularity`
no longer *supplies* the granularity (it only overrides). `--start`/`--end`
override as before.

### Tests

`trade-control-cli` (native): `trade_expiry_epoch` extraction (found / ignores
non-expiry time rules / none); `resolve_granularity` (defaults to plan / accepts
a matching override / rejects a mismatching override). `tv.rs` drops the
now-unused `resolution_to_friendly` and its tests.

### Follow-up

Rebuild + reinstall the `-dev` / `-staging` CLIs via the deploy scripts so the
installed `replay-candles-<env>` picks up the new defaults.

## v53 — 2026-06-22 — Bug #13: a resolve-failed cron-engine enter no longer retires the plan

### Why

A cron-engine H&S plan (`hs-nzd-chf-d12eb831`, NZD/CHF m15, 19-Jun 2026)
fired its single-shot `05-enter` on a tiny pinbar, the resolver produced a
degenerate zeros bracket (`trigger 0.0`, `sl 0`, R 0.0), and the worker
correctly rejected it `resolve-failed` — **but the FSM had already
transitioned `AwaitEntry → Done` on the same tick**, purely because the
once-enter *trigger* fired. The pure evaluator decides phase transitions at
fire-time and never sees the dispatch outcome; the state is persisted before
dispatch. So a doomed enter retired the whole plan, and its three veto rules
(`too-high`/`too-low`/`trade-expiry`, valid ~11h longer) stopped being
evaluated. No loss here (the plan held no position), but on a plan that *had*
opened a position an abandoned `close-positions` veto would be a missed
protective exit.

Linked Finding B: `run_enter` resolves **before** the `needs_golden` candle
gate, so a false-golden tiny pinbar (`signal_high ≈ signal_low`) fails resolve
first — which is why the log showed `resolve-failed` rather than
`needs-golden`. The engine FSM also didn't pre-gate `needs_golden`, so a
non-golden bar could fire the detector.

### What changed

- **Engine FSM pre-flight (`engine/src/evaluate.rs`).** A `PinePattern`
  (single-shot) enter is now pre-flighted before it fires/latches/retires the
  spine, via the new pure `pine_entry_dispatchable`:
  1. the candle-quality gate (`needs_golden`/`needs_confirmed` vs the latched
     signal flags — `None`/`false` both reject), and
  2. bracket resolution (`Resolved::from_intent` on the signal-folded shell).
  If either fails it's a **decline-this-bar** (stay `AwaitEntry`), not a
  `Done`. Both checks are pure and recompute identically on replay; the worker
  still re-runs its own gates + resolution on dispatch (this is a pre-flight,
  not a replacement — it never sees account caps / cooldown / retry /
  `allow_entry`).
- **Scope is `PinePattern`-only.** The M/W heartbeat enter is untouched — its
  resolution and by-design `NotArmedYet` decline are owned by
  `run_enter → maybe_update_mw_state`, so pre-resolving it in the FSM would
  wrongly suppress the heartbeat.

### Breaking

None. Pure-FSM behaviour change only; no wire-format, KV, or signed-field
change.

### Tests

`engine` crate (native): a resolvable Pine enter still fires + `Done`
(unchanged); an unresolvable bracket fires the detector but does **not**
retire the plan (phase stays `AwaitEntry`, enter doesn't latch); a
`close-positions` veto crossed *after* a resolve-failed enter still fires
(acceptance criterion 3); a `needs_golden` enter declines on a non-golden bar;
the M/W heartbeat is not pre-flighted; plus a direct unit test of
`pine_entry_dispatchable`. The four pre-existing Pine fixtures were given
real signal-anchored geometry (a bare no-geometry enter would now decline at
resolve).

### Follow-up

Finding B1 vs B2 (was the surfacing pinbar truly non-golden upstream, or did
Pine stamp a false `golden:1`?) is decided by the `ticks/` R2 object, not by
this change. This fix makes either case safe — a non-golden *or* unresolvable
bar now declines without retiring the plan — but if B2 holds, the Pine source
still wants the Bug #10-family fix separately.

## v52 — 2026-06-22 — Bug #12: continuous at-entry too-low/too-high enforcement

### Why

A live `too-low` veto failed to block a confirmed H&S entry (NZD/CAD,
−110.53 GBP, 10–11 Jun 2026). Root cause is a semantic gap from the engine
migration: the legacy TradingView `too-low`/`too-high` alert *wrote a
persistent KV veto* that a later confirmed enter found and rejected. The
engine re-modelled those as one-shot cross-event guard rules — the KV veto is
only written when price *crosses* the level on a closed candle. A gap past the
level, a level already breached when the plan armed, or a cross during a
disarmed phase writes **no** veto, so the enter confirmed and the order was
placed. `too-low` is really a *continuous* predicate ("is the entry already
past the pcl-exhausted level?"), not a one-shot cross.

### What changed

- **`Intent.entry_level_vetos: Vec<EntryLevelVeto>`** (new core type
  `EntryLevelVeto { name, level, past: VetoSide{Below,Above} }`). Baked onto
  the H&S/IH&S enter at arm time: `too-low` = pcl-exhausted (from the fib),
  `too-high` = invalidation (the right-shoulder horizontal), with sides
  derived from direction.
- **Worker gate (`run_enter`).** After resolving and before `allow_entry`, the
  worker rejects the entry when the resolved entry/trigger price is already
  past any baked level — `rejected: veto-active (<name>)`, HTTP 412, no order
  placed — independent of any cross-event guard. Byte-identical outcome string
  to the legacy KV veto path (a seen-id `Skip`, so the id isn't poisoned).
- **Engine cross-guard left as-is** (lowest risk): the new at-entry check is an
  *additional*, authoritative safety net, not a replacement.
- **Simulator** (`engine::simulator`): new `SimOutcome::Declined { name }`;
  `simulate_fill` short-circuits to it when the entry is past a level, so
  tick-replay reproduces the worker's gate (loss → no-fill).

### Config

- New signed field `Intent.entry_level_vetos` and `TradeSpec.entry_level_vetos`,
  both `#[serde(default, skip_serializing_if = "Vec::is_empty")]` — old signed
  intents / stored plans / spec yaml deserialise and round-trip unchanged.

### Tests

- core: `EntryLevelVeto::is_past` truth table (inclusive at the level) + JSON
  round-trip; `ResolvedEntry::reference_price`.
- tv-arm: `hs_entry_level_vetos` sides + skip-missing for short & long.
- cli: `build_enter_alert` carries the levels onto the enter intent.
- engine: tick-replay flips the −110.53 loss path to a clean no-fill when the
  pcl level is breached (and still fills when the entry is short of it).

### Rollout

Deploy only — no re-arm migration. The level is baked at arm time, so only
plans armed after deploy carry it; in-flight plans keep the cross-guard until
they expire/re-arm. Dev only (`./deploy-dev.sh`); do not redeploy staging
mid-week.

## v51 — 2026-06-22 — M/W: optional drawn right shoulder (4-point path) arms immediately

### Why

A 3-anchor M/W path (runup-start, left shoulder, neckline) leaves the right
tower unknown at arm time, so the worker has to *discover* it live — waiting
for a right-tower-reach confirmation and then a 50% "middle of the M"
downward cross before it arms. When the operator can already *see* the right
shoulder on the chart, that wait is needless latency: the pattern is valid the
moment both towers exist. The operator wanted to draw the right shoulder and
have the trade arm straight away, re-measuring each bar and aborting only on
the 1.3 break.

### What changed

- **Optional 4th path anchor `D` = right shoulder.** `tv-arm` now accepts a
  3- *or* 4-anchor M/W PATH. A 4th anchor is read as the right shoulder and
  baked onto the enter intent (`MwParams.right_shoulder: Option<f64>`).
- **4-point paths arm immediately.** With a right shoulder present the worker
  skips the live right-tower-reach and 50%-mid-cross gates (`from_mw_intent`);
  only the 1.3-extension ceiling and the stop-on-correct-side placement check
  remain. 3-anchor behaviour is unchanged.
- **Highest-shoulder geometry.** The SL anchor, the 1.3 cancel ceiling, and the
  `mw-cancel` / `mw-overshoot` veto levels are measured off the **higher** of
  the two shoulders when `D` is drawn (M: max; W: min). The worker still
  re-measures every bar — a higher shoulder reshapes the geometry via `MwState`
  and the 1.3 ceiling still aborts.
- **Arm-time validity.** `tv-arm` rejects a 4-point drawing whose right
  shoulder is on the wrong side of the neckline, or whose taller shoulder
  breaches the 1.3 extension of the *shorter* shoulder.

### Config

- New signed field `MwParams.right_shoulder` (and CLI `MwSpec.right_shoulder`),
  both `#[serde(default, skip_serializing_if = "Option::is_none")]` — a
  3-anchor signed intent / spec yaml stays byte-identical.

### Tests

- core: 4-point arms-without-mid-cross (M+W), 1.3 ceiling tracks the higher
  shoulder, wrong-side stop still declines, drawn shoulder seeds `MwState`.
- tv-arm: `validate_right_shoulder` (valid / 1.3-break / wrong-side, M+W),
  `highest_shoulder`, 4-anchor classification + pipeline accept/reject.

## v50 — 2026-06-22 — `plan show` finds archived (terminated) plans

### Why

`plan list --include-archived` would list a terminated plan, but
`plan show <trade_id>` for that same id returned **404 — no registered plan**.
A terminated plan usually exists *only* in the archive keyspace (its live
`plan:` / `plan-state:` rows are dropped on the terminal tick), and
`handle_plan_show` only scanned the **live** plans (`list_all_trade_plans`),
never the archive — so the one path the operator would use to inspect a
finished plan couldn't find it. (`plan delete` already scanned both.)

### What changed

- **`plan show` now scans live *and* archived plans.** A new pure,
  `StateStore`-generic helper `collect_plan_details(store, target)` gathers
  matches from the live rows first, then the archive; `handle_plan_show` 404s
  only when both are empty.
- **An archived match carries an `archived_at` field** in the dump (mirrors
  `PlanSummary::archived_at`), so the operator can tell at a glance whether
  `plan show` surfaced a live or a finished plan. Live matches omit it.

### Breaking

None. Live `plan show` output is unchanged (no `archived_at` field emitted);
the field appears only for archived matches.

### Tests

New `plan_show_tests` module (uses core's `MemStateStore` via the
`test-support` feature, added as a dev-dependency): an archived-only plan is
found and flagged with `archived_at`; a live plan is still found and *not*
flagged; an unknown id yields no details (→ 404 at the caller).

## v49 — 2026-06-20 — replay-candles: Brisbane-time output + clearer --source help + dev deploy

### Why

The replay report printed every candle/fill/exit timestamp in **UTC**, but the
operator (and the broker, and the TradingView chart they armed from) all work in
**Brisbane time**. Cross-referencing a fire against the chart meant doing +10h
arithmetic in your head. Separately, `--source`'s help implied it might bypass
the cache — it never did; **both** sources always go through candle-cache.

### What changed

- **All report timestamps now render in Brisbane time (UTC+10)** with an
  explicit `+10:00` suffix — candle fire times, fill/SL/TP times, and the
  "pulling candles" log line. Brisbane has no DST, so the offset is fixed
  year-round. New `replay_candles/brisbane.rs` (`bne()`); the candle *data* and
  the engine still compute in UTC internally — this is display-only.
- **`--source` help clarified.** Both `tradenation` and `oanda` always pull
  through candle-cache (filling the on-disk cache and cutting future broker
  calls); `--source` only selects the broker, never whether the cache is used.
  No behaviour change — wording only.
- **`replay-candles` now installs via `./deploy-dev.sh`** (and staging) as
  `replay-candles-<env>`. It's a second binary of the `trade-control-cli`
  package, so it already built with the others; added to `CLI_BINARIES` so the
  suffixed copy lands in `~/.cargo/bin`. It has no baked webhook (it talks to
  TradingView + the broker, not the worker), so the per-env copy is just a
  naming convenience.

### Breaking

None. Output format of timestamps changed (UTC → Brisbane), but no flags or
APIs changed.

### Config

None.

### Tests

`brisbane.rs`: UTC→Brisbane render (`11:00Z` → `21:00 +10:00`) and a
date-rollover case (`20:00Z` → next-day `06:00 +10:00`). Full bin suite (17)
and workspace green; wasm worker build stays ring-free.

### Follow-up

- Still could auto-derive `--source` from the TV chart exchange
  (`OANDA:`/`TRADENATION:`); deferred (carried from v47).

## v48 — 2026-06-20 — tv-arm: `--plan-out` builds the plan on its own

### Why

`tv-arm --plan-out plan.json` silently wrote nothing unless `--register-plan`
was *also* passed. The plan-build + JSON-dump lived entirely inside
`register_trade_plan`, which only ran under the `if args.register_plan` guard.
So the documented replay workflow (v47 TODO: "`tv-arm --plan-out plan.json`
builds the plan", then `replay-candles --plan plan.json`) didn't actually work
standalone — the operator got a clean exit and an empty `out_dir`, no file, no
warning.

### What changed

- The plan-build block now runs when **either** `--register-plan` **or**
  `--plan-out` is set. Used alone, `--plan-out` builds the `TradePlan` (control
  rules folded in), writes the pretty JSON, and stops — **no worker POST**.
  Combined with `--register-plan` it additionally registers the plan, exactly as
  before.
- `--update` re-arm (plan delete) still only fires under `--register-plan` —
  there's nothing to reconcile on the offline path.

### Breaking

- None. `register_trade_plan` gains a `register: bool` parameter that gates the
  worker POST; the offline path returns early after the optional disk write.

### Config

- No new flags. `--plan-out`'s doc comment no longer claims it's "only
  meaningful with `--register-plan`".

### Tests

- Existing `built_plan_round_trips_through_plan_out_json` covers the JSON shape;
  all 171 tv-arm tests pass. (The guard split is control-flow only.)

### Follow-up

- None.

## v47 — 2026-06-20 — replay-candles: pull the replay window straight from TradingView

### Why

The `replay-candles` workflow (v43/v45) required the operator to hand-type
`--instrument`, `--granularity`, `--start`, and `--end`. But the operator is
already *looking at exactly that window* in TradingView replay mode: they
rewind, arm the plan with `tv-arm`, then scrub the chart forward to the end of
the trade. At that point the chart's visible region **is** the window to replay,
and the chart symbol + resolution **are** the instrument + granularity.
Re-typing them is error-prone busywork.

### What changed

- `replay-candles` now reads the instrument, granularity, and start/end window
  off the **current TradingView chart** when those flags are omitted, via the
  same `trading-view` MCP wrapper `tv-arm` uses (`TvMcp::get_state` →
  symbol + resolution, `TvMcp::get_range().visible_range.to_utc()` →
  start/end).
- All four flags remain **optional overrides** — any flag that is passed wins
  over the chart value. With all of instrument/granularity/start/end explicit,
  no MCP call is made at all.
- New `--tv-mcp-root` flag (mirrors `tv-arm`) to point at a non-default tv-mcp
  checkout.
- The chart resolution → granularity map (`"60"` → `1h`, `"D"` → `1d`, …)
  mirrors `tv-arm`'s `resolution_to_granularity`; an unsupported resolution
  (sub-minute, weekly) errors with a clear "set `--granularity` explicitly"
  message rather than guessing.

### Breaking

- `--granularity` and `--start` are no longer required / no longer defaulted to
  `1h`. Omitting them now pulls from TradingView instead of erroring (`--start`)
  or silently assuming `1h` (`--granularity`). Existing invocations that passed
  both explicitly are unaffected.

### Config

None.

### Tests

`cli/src/bin/replay_candles/tv.rs` unit tests: exchange-prefix stripping
(`OANDA:EURUSD` → `EURUSD`), TV-resolution → friendly-granularity mapping,
unsupported-resolution rejection, and a round-trip asserting every friendly
string this module emits parses back through the CLI's own granularity parser.
The live MCP path is not unit-tested (it shells out to node). Full workspace
suite green; wasm worker build stays ring-free (no `trading-view`/`candle-cache`
in the cdylib tree).

### Follow-up

- Could also derive the candle `--source` from the chart exchange
  (`OANDA:` → oanda, `TRADENATION:` → tradenation) instead of defaulting to
  TradeNation; deferred until there's a concrete need.

## v46 — 2026-06-20 — multi-shot retry gate: never stack a duplicate on a still-open position (Bug #11)

### Why

A multi-shot `enter` (`max_retries > 0`) re-fired while its **first
position was still open** and the worker placed a **second live
position** on the same `trade_id`/instrument/side — the account briefly
carried double exposure and took two stop-outs where the design allows
one (incident `hs-eur-cad-b6b708cc`, EUR/CAD, 2026-06-18, demo). The
retry gate is meant to reject a re-entry while a prior attempt is open;
it didn't.

Root cause, confirmed from the 18-Jun worker logs: the gate **did** run
and **did** find the prior attempt, but its per-attempt broker lookup
mis-resolved the still-open TradeNation position to `AttemptState::Unknown`.
On a bracketed TN entry the entry order executes and a **fresh** SL child
order is attached with a new id, so the live `Position.order_id` no longer
equals the originating entry order id we stored on the `EntryAttempt`.
`compute_attempt_state` matched only on that entry order id, missed the
position, and fell through to `Unknown` — which the gate bucketed with
the *closed* states and skipped past, proceeding to place the duplicate.
(`lookup_attempt_state` success is silent, which is why no lookup line
appears in the logs and an earlier read mistook this for "the gate never
checked".) The "1 → 0 tracked-attempts oscillation" in the cron logs was
a red herring: two workers (dev + staging) sweeping their own KV into one
log stream.

### What changed (three layers, defense in depth)

- **`Unknown` now fails safe → reject (412).** A prior attempt the
  broker can't confirm as open/pending/closed is treated as "might still
  be open" and blocks the re-entry, instead of being treated as done.
  New outcome string `rejected: prior-attempt-unknown`
  (`src/retry_gate.rs`).
- **`compute_attempt_state` correlates an open position on EITHER the
  stored entry `order_id` OR the snapshotted `position_id`** — so a
  still-open position whose live (bracket) order id has drifted is still
  recognised as `OpenPosition` once its PositionID has been snapshotted
  (`src/tradenation_adapter.rs`).
- **Independent open-positions backstop before placement.** When there
  is at least one tracked prior attempt, the gate lists the broker's live
  open positions for the instrument and rejects (412) if any correlates
  to a prior attempt by `order_id` or `position_id` — immune to the
  per-attempt-lookup taxonomy and to bracket order-id drift. New outcome
  string `rejected: trade-already-open (backstop)`. A transient failure
  reading the positions list fails safe (503) rather than risking a
  duplicate (`src/retry_gate.rs::open_position_backstop`).

### Behaviour

- A multi-shot `enter` re-fire while a same-`trade_id` position is open
  is now rejected (412), no second order placed.
- A re-fire **after** the prior attempt has provably closed/cancelled
  still re-enters (Bug #1 behaviour preserved — `ClosedWin` /
  `ClosedLossOrBreakeven` / `Cancelled` still fall through).
- The single-shot path (`max_retries: Static(0)`, the default) is
  untouched — the gate is skipped entirely, no new KV/broker calls.

### Breaking

None. New reject outcome strings only.

### Tests

- gate: `Unknown` prior attempt rejects (`prior-attempt-unknown`).
- gate backstop: rejects a live position matching a prior attempt by
  `position_id` even when the order id has drifted; ignores an unrelated
  same-instrument position; fails safe (503) on a transient positions
  read.
- adapter: `compute_attempt_state` resolves `OpenPosition` via a
  snapshotted `position_id` when the live `order_id` no longer matches.
- regression: collapsed states still proceed; single-shot baseline makes
  no new calls.

### Follow-up

- The closed-position `RefID`-vs-`PositionID` limitation on TradeNation
  (a stopped-out TN trade still resolves to `Cancelled`, Bug #1) is
  unchanged and out of scope here — the `Unknown` fail-safe and the
  backstop both guard the **open** path, which is what this incident hit.
## v45 — 2026-06-20 — `replay-candles --print-completions`

### Why

`replay-candles` (v43) shipped without the zsh completion flag the other
operator tools (`tv-arm`, `trade-control`) carry, so TAB-completing its flags
needed hand-written compdef.

### What changed

- **`replay-candles --print-completions`** emits the clap-generated zsh
  completion script (bound to the invoked binary name so a renamed-on-install
  copy completes for its own name), mirroring `tv-arm --print-completions`.
  Because `--plan`/`--start` are required, the flag is detected on the raw argv
  before `Args::parse()` so a bare `--print-completions` doesn't trip clap's
  required-arg validation.

### Breaking / Config

- None. New flag only; `clap_complete` was already a `cli/` dependency.

### Tests

- Verified standalone (no required args), exit 0, `#compdef replay-candles`,
  and that required-arg enforcement is unaffected on a normal run.

## v43 — 2026-06-20 — `replay-candles`: offline candle replay through the engine

### Why

We had no way to take a registered `TradePlan` and ask "what would the engine
have fired over this window, and would those entries have won or lost?" without
standing up a live cron trigger. The tax-tracker's `replay` only diffs recorded
`TickBundle`s; nothing pulled fresh candles for a plan + time range.

The obvious shape — "POST candles into local `wrangler dev` with a mock broker"
— doesn't fit: the worker has no candle-ingest endpoint (the cron engine *pulls*
candles each tick), and the order-dispatch path can't run off-wasm (`run_enter`
builds a `worker::Response` that panics at construction). But the decision core
(`evaluate_plan`) is pure and native-callable, and `simulate_fill` is the
broker-free fill model. So the harness drives the pure core natively.

### What changed

- **New native bin `replay-candles`** (in the `cli/` workspace member):
  load a `TradePlan` JSON, resolve the instrument per-source via
  `instrument-lookup`, pull the candle window via `candle-cache`
  (TradeNation — matches the live engine — or OANDA, disk-cached), convert
  `candle_model::CandleData` → engine `Candle` (mid, UTC, drop volume), then
  seed-without-firing and feed closed bars through `evaluate_plan` one tick at a
  time exactly as `run_engine_tick` does. Each fired enter is run through the
  pure `simulate_fill` over the forward candles to report the fill/SL/TP
  outcome. No `wrangler dev`, no HTTP, no live orders.
- **`tv-arm --plan-out <path>`** writes the fully-built `TradePlan` (control
  rules folded in) as pretty JSON before the register intent consumes it, so the
  harness can load the exact plan the engine received. Only meaningful with
  `--register-plan`.

### Breaking

- None. `register_trade_plan` gains a `plan_out: Option<&Path>` parameter
  (internal to `tv-arm`).

### Config

- New `tv-arm` flag `--plan-out <path>`.
- New `replay-candles` env: `TN_ACCOUNT_TYPE` (`demo` default / `live`) with
  `TN_USERNAME`+`TN_PASSWORD` for live; `OANDA_TOKEN`+`OANDA_ACCOUNT_ID` for
  `--source oanda`.

### Build

- `candle-cache` / `oanda-client` / `candle-model` / `trade-control-engine` are
  added to `cli/` only. The worker cdylib does not depend on `cli`, so the wasm
  build is unaffected (candle-cache absent from the worker dep tree; wasm cdylib
  still builds).

### Tests

- New unit tests: granularity parse/bridge, instrument resolution, candle
  conversion+timezone, seed/loop wiring, datetime parsing, and a `tv-arm` plan
  JSON round-trip. End-to-end verified against the TradeNation demo feed.

### Follow-up

- A hand-authored firing-rule fixture to exercise the report's TP/SL counters
  end-to-end (the firing path is currently covered via the engine's own
  `evaluate_plan`/`simulate_fill` tests).
- Multi-granularity / HTF detector windows; a `--source both` divergence mode.

## v42 — 2026-06-19 — `on_too_close: limit` recovery (Step 4 of the too-close fallback)

### Why

The `on_too_close` stop-entry fallback (v17) shipped `skip` and `market` but
left `action: limit` as a stub that degraded to `skip` with the reason
`too-close-limit-unimplemented`. `limit` is the R-preserving recovery: when a
stop trigger has been overtaken by price (`#19-10`), instead of chasing the
move at market (`market`, which accepts a worse fill within a slippage bound),
rest a **limit at the original trigger** and wait for a pullback. A limit can't
fill worse than its price, so the planned R is preserved exactly — at the cost
of possibly never filling.

### What changed

- **`action: limit` is now implemented.** On a `#19-10` rejection of a stop
  whose fallback is `limit`, the worker re-places a **single** limit order
  resting at the original stop trigger (`src/too_close.rs` `TooClosePlan::Limit`
  + the new arm in `place_entry_too_close_fallback`, `src/lib.rs`). No fresh
  sizing — the entry reference is unchanged, so the original stop-distance /
  1%-equity math is reused.
- **Geometry guard.** A limit must rest on the correct side of the market
  (long: trigger at/below current price; short: at/above) or it would be a
  `#19-9` ("limit on the wrong side"). In a genuine `#19-10` the price has
  overrun the trigger so this holds; a degenerate / non-overrun case is skipped
  with `too-close-limit-wrong-side` rather than firing a doomed order.
- **No broker-native GTD needed.** TradeNation order placement is hardcoded
  GoodTillCancel upstream, but the recovered limit is recorded as an ordinary
  `EntryAttempt` (the existing success-path `record_placement`), so the cron
  sweep (`src/cron/sweep.rs`) cancels it on `attempt_expired` (`not_after`) or
  `bar_expiry_due` (`cancel_at`). The limit inherits the alert window's lifetime
  for free.
- **One attempt, not a loop** — identical to `market`. A broker reject returns
  the original `EntryError::EntryTooCloseToMarket`, so the seen-id is never
  poisoned and the next signal bar can retry.

### Breaking

None. `OnTooCloseAction::Limit` already existed and parsed; only its runtime
behaviour changed (was: skip; now: limit re-place). The `TooClosePlan` enum
gained a `Limit` variant (exhaustive matches in this crate updated).

### Config

No wire-format change. `on_too_close: { action: limit }` was already accepted;
`max_slippage_pips` is not required or used for `limit`.

### Tests

`src/too_close.rs`: long/short correct-side → `Limit`, long/short wrong-side →
`Skip { too-close-limit-wrong-side }`, exact-trigger equality rests, non-finite
price skips (replaced the old `limit_action_skips_until_implemented` test).
Worker suite green.

### Follow-up

None outstanding for the too-close fallback — all of
`BUG-entry-too-close-to-market.md`'s suggested steps (1 plumb, 2 wire format,
3 market, 4 limit) are now shipped.

## v41 — 2026-06-19 — archive terminal plans for post-mortem (`plan list --include-all`)

### Why

When a registered plan reached a terminal phase (a veto fired, or the
single-shot entry was dispatched) the cron engine deleted both its `plan:` and
`plan-state:` KV rows on that tick (`src/cron/engine.rs` `persist_plan_state`).
`plan list` scans the `plan:` prefix, so a vetoed/completed plan vanished — there
was no way to list it afterward to analyze why it terminated.

### What changed

- **Archive instead of plain delete.** On the terminal cron tick the engine now
  snapshots the finished plan (plan body + terminal `PlanState`) to a new
  `archived-plan:{scope}:{trade_id}` KV key *before* clearing the live rows. A
  failed archive is logged but doesn't fail the tick.
- **`plan list --include-all`** (alias `--include-archived`) also lists archived
  plans; plain `plan list` still shows only live plans. New `ARCHIVED` column
  carries the archive timestamp (blank for live plans).
- **`plan delete <trade_id>`** now also clears any matching `archived-plan:` row
  — so a terminated plan (which usually exists *only* in the archive) is
  deletable after analysis. Still idempotent.
- **No TTL on archived plans** — they persist until `plan delete`. Documented as
  a manual-cleanup keyspace.

### Breaking

- `cli::build_plan_list_intent` gained a third `include_archived: bool` argument.
- `StateStore` gained three methods: `archive_plan`, `list_all_archived_plans`,
  `clear_archived_plan` (implemented for the KV store and the in-memory test
  store).

### Config

- New signed top-level `Intent.include_archived: bool` (default false, elided
  when false → wire form byte-identical for existing `plan-list` intents).

### Tests

- `memstore_archived_plan_round_trips_lists_and_clears` — archive round-trip,
  scope recovery from the key, terminal-state capture, list, and scoped clear.

### Follow-up

- The `trading-tax-tracker` R2 consumer reads the `req/`/`ticks/` prefixes, not
  KV — it has no view of archived plans. If post-mortem tooling wants the
  archive, expose it via a read path there.

## v40 — 2026-06-18 — `tv-arm --update`: re-arm an existing engine plan

### Why

`tv-arm` mints a fresh random `trade_id` every run, so re-arming a setup (move
the annotations, re-run) registers a *new* plan while the old one keeps ticking
in KV until its TTL. The operator's manual flow ("delete the TV alerts, re-run")
had no engine-side equivalent — stale plans accumulated.

### What changed

- **`tv-arm` — `--update [trade-id]` flag** (only with `--register-plan`).
  Before registering the fresh plan it deletes the prior one from the engine:
  - bare `--update` auto-resolves by instrument — POSTs `plan-list`, and if
    exactly one plan is registered for this instrument deletes it; none → no-op;
    more than one → hard error naming the candidates (re-run with an explicit
    id).
  - `--update <trade-id>` deletes exactly that plan.
  Reuses the `plan-delete` action (clears `plan:` + `plan-state:`). Leaves
  TradingView alerts untouched — engine-only reconciliation.
- **`tv-arm` — `post_intent_blocking`** returns the worker's response body (so
  the `--update` flow can read the `plan-list` YAML); `post_register_blocking`
  is now a thin wrapper over it.

### Tests

- `tv-arm`: `resolve_update_target` — explicit id verbatim; auto single-match;
  auto no-match no-op; auto multi-match hard error (names candidates); bare
  `--update` (`""`/whitespace) treated as auto. tv-arm 158 green; clippy + fmt.

### Follow-up

- The actual POSTs (`plan-list` / `plan-delete`) are network-bound and aren't
  unit-tested in `update_existing_plan`; the pure `resolve_update_target` is.
  End-to-end is exercised on the staging worker during re-arm.

## v39 — 2026-06-18 — Calendar / news bars folded into the registered plan

### Why

`tv-arm --register-plan` produced a server-side `TradePlan` with **no**
pause/resume/news-start/news-end rules, while `--create-alerts` correctly
created those calendar bars as TV alerts. So a registered plan silently dropped
every blackout / news window — the engine never paused entries around a CPI
print, never opened the news-window gate. Root cause: `register_trade_plan` ran
at pipeline step 5b, *before* the pause/news/calendar bundles were built (steps
6–8), and `build_trade_plan` only walked `built_trade.alerts` (veto/prep/enter/
close). Two further gaps compounded it: the engine had no evaluation path for
control rules, and the cron dispatcher rejected their actions.

### What changed

- **`engine` — non-terminal control rules.** New `evaluate_controls` pass in
  `evaluate_plan` (runs before the guards) fires Pause/Resume/NewsStart/NewsEnd
  rules on their `TimeReached` trigger, dispatches the carried intent, and
  latches — but, unlike a guard, never sets `Phase::Done`, so the trade's spine
  keeps running. Armed in every phase (a window can open before break-and-close).
  New `is_control_rule` helper.
- **`worker` cron — dispatch the control fires.** `dispatch_action`
  (`src/cron/engine.rs`) routes the four control actions to the same
  `handle_pause` / `handle_resume` / `handle_news_start` / `handle_news_end` the
  webhook uses (KV-only, no broker), replacing the previous
  `unsupported-action` rejection. Shadow plans still log-only (the `tick_one`
  shadow path returns before dispatch).
- **`tv-arm` — fold the bundles in.** `register_trade_plan` moved to *after* the
  pause/news/calendar bundles are built, and new `append_control_rules`
  (`trade_plan_build.rs`) appends one `TimeReached` `ConditionRule` per bundle
  alert — carrying that alert's signed intent verbatim, anchored to the window's
  start/end edge. Covers the operator's chart-drawn pairs (`BuiltPause`/
  `BuiltNews`) and the auto-fetched forex-factory events. The dead
  `roles.*_pairs.first()` arms in `trigger_for` are removed (they only ever saw
  one pair, and these basenames never appear in `built_trade.alerts`).
- **`cli` — surface the built bundles.** `run_calendar_bars` now returns
  `Vec<BuiltCalendarBundle>` (the in-memory `BuiltPause`/`BuiltNews` it already
  builds), so the register path reuses them rather than re-parsing the signed
  YAML.

### Breaking

- `cli::run_calendar_bars` returns `Result<Vec<BuiltCalendarBundle>>` (was
  `Result<()>`). The standalone `calendar-bars` bin ignores it.
- `tv-arm::register_trade_plan` gains pause/news/calendar bundle params
  (internal).

### Tests

- `engine`: pause fires at its epoch without ending the spine (enter heartbeat
  still fires the same bar); pause+resume fire on their own bars and don't
  refire; two news windows → all four fires.
- `tv-arm`: `append_control_rules` over one chart pause + one news + one
  calendar event yields 8 control rules with the right actions and window-edge
  epochs.
- Full workspace green (engine 35, worker 200, cli 239+13, tv-arm 153); clippy
  native + wasm32 + fmt clean.

### Follow-up

- The engine-side control dispatch is wasm-bound (`Env` / `worker::Response`),
  so it has no native unit test — verified by the worker compiling and the demo
  parallel run (the Stage F gate), same as the rest of the cron dispatch path.

## v38 — 2026-06-18 — Trim no-op engine ticks from the R2 `ticks/` recording

### Why

The cron engine recorded a full `TickBundle` on **every** tick that saw a new
closed bar — even when that tick changed nothing (no intent fired, no phase
transition, plan not done, KV write OK). Those "no-op" bundles aren't compact:
each re-stores the whole `plan: TradePlan`, both the `prior` and `new` `PlanState`s,
*and* the wide `detector_window` slice. Over a long-running pattern with a quiet
entry phase (e.g. an H&S waiting for break-and-close), that's one fat,
near-duplicate object per bar carrying no information. This stops recording them
while keeping a lightweight trace so a silent gap in the `ticks/` stream is never
mistaken for "the cron stopped".

### What changed

- **New pure predicate `PlanEval::is_noteworthy(&prior)`** (`core/src/plan_eval.rs`):
  a tick is noteworthy if it `fired` anything, finished the plan (`done`), or the
  FSM's *meaningful* state advanced vs the prior.
- **New helper `PlanState::advanced_vs(&prior)`** (`core/src/plan_state.rs`):
  compares only the FSM-meaningful fields — `phase`, `fired`, `break_close_at`,
  `retest_seen_at`, `mw`. It deliberately **ignores** `watermark`, `expires_at`,
  and `last_close`, all of which churn on essentially every tick (a whole-struct
  `!=` would make nothing a no-op).
- **Live + shadow record sites gated** (`src/cron/engine.rs`): both now call
  `record_tick_to_r2` only when `eval.is_noteworthy(&prior)`; otherwise they emit
  a single heartbeat `rlog!` and skip the write. The **put-failed** site is
  unchanged — a failed transition (`success:false`) is always recorded.

### Behaviour (visible)

- **Recording volume drops**: no-op ticks no longer produce R2 objects. The
  `ticks/` prefix now holds only ticks where something fired / finished /
  advanced, plus failed-KV-transition bundles. Each no-op leaves a heartbeat log
  line (visible in Cloudflare Real-time Logs) instead.
- **No change** to what a noteworthy bundle contains, to dispatch, or to state
  persistence — KV is written every tick as before; only the *recording* is
  trimmed. Replay is unaffected: each recorded bundle is self-contained
  (carries its own `prior_state`), and the next noteworthy bundle reloads the
  up-to-date `last_close` from KV.

### Config

- None.

### Tests

- `core`: `advanced_vs` unit tests — identical state, watermark-only,
  expires_at-only, and `last_close`-only changes are all **not** advances;
  phase / fire-latch / break_close / retest stamps **are**.
- `engine`: `is_noteworthy` against real `evaluate_plan` output — fired,
  finished, and phase-advance are noteworthy; the **critical**
  `not_noteworthy_on_watermark_only_advance` proves a new-bar-but-nothing-moved
  tick is a no-op (and that a full-struct compare *would* wrongly call it
  changed).

### Follow-up

- The `tick_bundle_noop_trim_idea` memory is now implemented; the heartbeat-log
  half of that idea ships here too.

## v37 — 2026-06-18 — Retire the `trade-control replay` subcommand (replay moves to `trade-analyzer`)

### Why

Replay's single home is now the `trade-analyzer` CLI (in the
`trading-tax-tracker` repo) — that's the downstream R2-recording consumer, so
replay sits next to the bundle/timeline tooling and gained an `--from-r2` fetch
there (its v42). `trade-control` shipped a `replay` subcommand in v33 as the
first landing spot; keeping a second copy here just risks the two drifting.
This removes the duplicate. `trade-control` keeps what it *uniquely* owns: the
worker that **writes** tick-bundles, and the `TickBundle` / `evaluate_plan` /
`simulate_fill` library types that `trade-analyzer` consumes as path-deps.

### What changed

- **Removed `trade-control replay`** — deleted `cli/src/replay.rs`, its
  `mod replay;` + `pub use replay::{…}` re-export from `cli/src/lib.rs`, and the
  `Replay(ReplayArgs)` command variant + `Cmd::Replay` dispatch arm in
  `cli/src/bin/trade_control.rs`.
- **Dropped the `trade-control-engine` dependency from the `cli` crate** — it
  was pulled in only for `simulate_fill`/`evaluate_plan` in the replay path.
  The `engine` crate itself is unchanged; `trade-analyzer` depends on it
  directly.

### Breaking

- `trade-control replay` no longer exists. Use `trade-analyzer replay` (same
  bundle format; adds `--from-r2 <key>`).

### Config

- None.

### Tests

- No behaviour code changed; the migrated replay tests live in `trade-analyzer`.
  Workspace tests / clippy `-D warnings` / fmt / wasm worker build remain green.

### Follow-up

- The candle-cache → ReplayBroker historical walk and multi-tick replay land in
  `trade-analyzer`, not here.

## v36 — 2026-06-18 — Detector window reaches back to the earliest trendline anchor

### Why

The v34/v35 `bar_seconds` fallback fired whenever a trendline anchor fell
outside the engine's fetched candle window — and for a non-Pine plan (pure
M/W or trendline-only H&S preps) the window was just the watermark-bounded
`fresh` slice, so **any** anchor older than the last cron gap was out-of-window
and resolved by the wall-clock divisor. v35 made that path observable; this
removes the cause. The real fix is to fetch enough history that anchors are
always in-window, making the bar-index count exact and the fallback dead code
for a normally-armed plan.

### What changed

- **worker — widen `detector_window_for`.** The window start is now the
  earliest `since` any consumer needs, fetched once: the existing
  `PinePattern` lookback (`min_lookback_bars` behind the freshest candle) **and**
  the earliest `TrendlineCross` anchor across all the plan's rules (minus one
  bar of slack so the anchor's own bar is in-window). Split into two pure
  helpers — `pine_lookback_since` and `trendline_anchor_since` (the latter over
  a free `earliest_trendline_anchor_epoch(triggers)` so it unit-tests without
  building `Intent`s). A plan with neither a Pine entry nor a trendline (a pure
  M/W heartbeat) keeps the no-extra-fetch `fresh`-only fast path.

### Breaking

- None. Behaviour change only: trendline plans now fetch a wider back-window
  (one extra broker candle call covering history to the earliest anchor),
  removing the out-of-window fallback for normally-armed plans. The v35 warning
  surface stays as the belt-and-braces signal for a pathological anchor.

### Tests

- `earliest_trendline_anchor_epoch`: min across multiple trendline rules;
  `None` for a no-trendline plan; reversed (b < a) endpoints pick the true min.

### Follow-up

- The `bar_seconds` field + fallback divisor are now exercised only by a
  pathological anchor older than the fetch reaches. Could be retired entirely if
  the warning never appears on a live plan over a meaningful window — left in as
  a safety net for now.

## v35 — 2026-06-18 — Trendline `bar_seconds` fallback is now observable, not silent

### Why

v34 made trendline crosses interpolate in bar-index space, with a `bar_seconds`
wall-clock divisor *only* as a fallback for an anchor that falls outside the
fetched candle window. That fallback was correct but **silent**, and it has two
sharp edges worth surfacing: (1) it re-introduces wall-clock spacing across any
closed session in the *un-fetched* span (the exact assumption the bar-index work
removed), and (2) on a plan signed before the `bar_seconds` field existed
(`bar_seconds = 0`) an out-of-window anchor makes the trendline silently
**un-evaluable** — it just never fires, with no trace. Both are rare (a normal
H&S/M/W `detector_window` straddles its anchors) but a silent degraded path is
exactly the kind of thing that costs a debugging session later.

### What changed

- **`engine` — pure warning surface.** New `trendline_anchor_warnings(plan,
  window)` classifies each `TrendlineCross` anchor against the window
  (in-window / extrapolated / unresolvable) and returns human-readable
  diagnostics. `evaluate_plan` attaches them to the new `PlanEval.warnings`
  field. Pure and window-derived, so it recomputes deterministically on replay.
- **`core` — `PlanEval.warnings: Vec<String>`.** `#[serde(default,
  skip_serializing_if = "Vec::is_empty")]` — old tick bundles still deserialise,
  and a clean tick adds nothing to the recorded JSON.
- **worker — log them.** `run_engine_tick` (`src/cron/engine.rs`) `rlog!`s each
  warning (`cron engine: plan <id> trendline …`) so the degraded path shows up
  in Cloudflare Real-time Logs instead of being invisible. Logged for both live
  and shadow plans, before dispatch.

### Breaking

- None. `PlanEval` gains a defaulted field; the replay diff still compares only
  `fired` / `new_state` / `done` (warnings recompute from the same recorded
  inputs, so they are deliberately *not* diffed).

### Tests

- `engine`: in-window anchors warn-free; an out-of-window anchor warns about the
  `bar_seconds` extrapolation; `bar_seconds = 0` warns "unresolvable / won't
  fire"; both-anchors-out warns twice; a non-trendline (M/W) plan never warns;
  end-to-end `evaluate_plan` surfaces the warning on `PlanEval.warnings`.

### Follow-up

- The real fix for a warning in the logs is to **widen the candle fetch** in
  `detector_window_for` so anchors are always in-window — which would make the
  `bar_seconds` fallback (and these warnings) dead code. Deferred until the logs
  show it actually happening on a live plan.
## v34 — 2026-06-18 — Trendline crosses evaluated in bar-index space, not wall-clock

### Why

The engine interpolated a neckline's price between its two anchors by **elapsed
wall-clock seconds**, so the line kept sloping through nights, weekends and
exchange closures. TradingView's x-axis is *ordinal* — closed sessions aren't
plotted, so a trendline advances one step **per traded bar**, not per second.
For any gapped instrument (everything but 24/5 FX, and even FX gaps at the
weekend) the engine resolved the `03-prep-break-and-close` / `04-prep-retest`
level at the wrong price. Confirmed on live TradeNation data: ALPHABET's hourly
feed shows only the ~7 cash-session bars per day, eliding the 18 h overnight gap
and the 66 h weekend gap to single bar steps — exactly what TV draws and exactly
what wall-clock interpolation got wrong.

### What changed

- **`engine` — bar-index interpolation.** `line_price_at` now measures a
  candle's position along the line as a fraction of *bars* between the anchors,
  counting the bars actually present in the broker feed (`detector_window`;
  gaps are absent). New `bar_index_at` resolves an epoch → (fractional) bar
  index: exact bar match, interpolation across a one-bar data hole, or
  `bar_seconds`-based extrapolation when an anchor sits outside the fetched
  window. `eval_trigger` (+ `fire_rule` / `stamp_retest` / the spine
  evaluators) gains a `window: &[Candle]` param, ignored by every non-trendline
  trigger.
- **`core` — signed `bar_seconds`.** `Trigger::TrendlineCross` gains
  `bar_seconds: i64` (`#[serde(default)]` → `0` = "pure bar-count, no fallback"
  on plans signed before this field). It rides the existing whole-body HMAC, so
  it can't be tampered.
- **`tv-arm` — bake it.** `trendline_trigger` stamps `granularity.seconds()`
  onto each trendline (threaded through `build_trade_plan` → `build_rule` →
  `trigger_for`).

### Breaking

- `eval_trigger` signature gains a trailing `window: &[Candle]` (engine-internal;
  no external callers).
- `Trigger::TrendlineCross` gains a `bar_seconds` field (additive, defaulted).

### Tests

- `engine`: `trendline_gap_uses_bar_index_not_wall_clock` (the bug — a 23 h gap
  between bar 1 and bar 2 must NOT slide the line; the level at bar 1 is the
  bar-index half-way, not the wall-clock ~4 %), `trendline_interpolates_level_at_bar_index`,
  reworked `trendline_respects_extend_forward_false` onto a real bar window.
- `core`: `trendline_missing_bar_seconds_defaults_to_zero`.
- `tv-arm`: existing H&S plan test now asserts `bar_seconds: 3600` baked from
  the H1 chart.
- Full workspace green; clippy + fmt + wasm32 clean.

### Follow-up

- The `bar_seconds` fallback only triggers when an anchor predates the engine's
  fetched candle window; in practice `detector_window` reaches back far enough
  that the exact-bar-count path is used. If a long-lookback neckline ever needs
  the engine to fetch deeper history, that's a candle-fetch widening, not a
  geometry change.

## v33 — 2026-06-17 — Engine tick-bundles: record cron ticks to R2 + native replay

### Why

After the rearchitecture the cron engine — not an inbound TradingView alert — is
where every trading decision happens (it loads each registered `TradePlan`,
pulls fresh candles, runs the pure `evaluate_plan`, dispatches the fired
intents). But the tick recorded **nothing**, so there was no way to replay a
real engine decision offline. This collapses the bug-fix loop from a week on
demo to a second in CI: fix a bug, replay the tick that showed it, watch the
outcome change.

### What changed

- **`TickBundle`** (`core/src/tick_bundle.rs`) — a self-contained,
  serde-round-trippable record of one `(tick, plan)`: the full `evaluate_plan`
  input tuple (`plan`, prior `PlanState`, `new_candles`, detector window,
  `now`/`expires_at`) + golden `PlanEval` output + per-fire `DispatchOutcome`s +
  the plan-state `KvTickTransition` (before/after/success/error).
- **Recording** — the cron tick now writes a bundle per evaluated plan to R2
  under a new **`ticks/<date>/<tick_ts>-<trade_id>.json`** prefix (sibling to
  `req/`, same `TRADE_CONTROL_R2` bucket), fire-and-forget via `ctx.wait_until`,
  fail-soft on every axis (`src/tick_recording.rs`). Both shadow and live ticks.
- **`trade-control replay <bundle.json>`** — re-runs the same `evaluate_plan` and
  diffs `fired`/`new_state`/`done` against the recorded `eval`; non-zero exit on
  mismatch (CI gate). `--simulate` additionally resolves each fired enter and
  walks the candle path through a dumb broker-simulator
  (`engine/src/simulator.rs`), reporting filled / stopped-out / took-profit /
  never-filled.

### Breaking

- `FiredIntent` / `PlanEval` definitions moved from `trade-control-engine` to
  `trade_control_core::plan_eval` (re-exported from `engine`, so `evaluate_plan`'s
  signature is unchanged). `Candle`, `LatchedSignal`, `FiredIntent`, `PlanEval`
  gained `Serialize`/`Deserialize`.
- `run_engine_tick` / `tick_one` now take the cron `ScheduleContext` (was dropped
  as `_ctx`).

### Config

- New R2 prefix `ticks/`; no new bindings (reuses `TRADE_CONTROL_R2`).
- `trade-control-core` gains a `test-support` feature exposing `MemStateStore`
  (pulls `serde_json` + `chrono/clock`); off by default, never in the wasm build.

### Tests

- `TickBundle` JSON round-trip + `r2_key` layout (core).
- `replay`: faithful bundle → MATCH, tampered → MISMATCH (cli).
- Broker-simulator fill/exit paths: TP, SL, never-filled, filled-open, ambiguous
  → pessimistic-stop (engine).

### Follow-up

- Replaying the recorded `dispatch_outcomes` through the real `run_enter` /
  `run_close` handlers needs the deferred `worker::Response` → `{status,message}`
  decouple (those handlers live in the worker cdylib and panic off-wasm). The
  pure-evaluation diff + price-path simulation are the phase-1 workhorse.
- Wiring the downstream `trading-tax-tracker` to read `ticks/` as a sibling to
  its `req/`-based `bundle` command.
- Multi-tick replay (glob a trade's whole `ticks/` prefix in sequence) for the
  full fill story across ticks.

## v32 — 2026-06-17 — `trade-control plan list` / `plan show` (inspect registered engine plans)

### Why

There was no way to see what the server-side engine is evaluating. During the
engine's parallel-run period (shadow mode, v31) the operator needs to confirm a
plan actually registered, whether it's in shadow or live mode, and how far its
FSM has progressed — without grepping Cloudflare logs.

### What changed

- Two new read-only control actions: **`plan-list`** (every registered plan +
  a compact summary of its `PlanState`) and **`plan-show`** (one plan dumped in
  full — every rule + its persisted state, target named by `trade_id`, scanned
  across all account scopes). KV-only, idempotent, signed like `status`.
- Worker handlers `handle_plan_list` / `handle_plan_show` (`src/lib.rs`) reuse
  the existing `list_all_trade_plans` + `get_plan_state` store methods. New
  `PlanSummary` / `PlanDetail` view structs.
- CLI **`trade-control plan list`** (aligned table) and **`trade-control plan
  show <trade_id>`** (per-match header + YAML), each with **`--yaml`** for the
  raw worker response. Builders `build_plan_list_intent` /
  `build_plan_show_intent` (`cli/src/control.rs`).

### Config

- New CLI subcommand group `trade-control plan {list,show}`. No new secrets.

### Tests

- CLI: `plan_list_table_aligns_and_fills_missing`, `plan_list_empty_is_friendly`,
  `plan_show_labels_each_match` (pure formatting). Core/worker exhaustiveness +
  build covers the new `Action` variants.

### Note

- Also folded in the pending `cli/src/lib.rs` rustfmt diff left over from the
  market-info merge (the re-export block this change already edits).

## v31 — 2026-06-17 — Engine shadow mode (observe-only plans for the safe parallel run)

### Why

The server-side engine dispatches a registered plan's fired intents through the
*same* `run_enter` / `run_close` / veto handlers the webhook uses. So a live
(non-shadow) registered plan would place **real broker orders in parallel with
the live TradingView alerts** — double-firing every setup. But the Stage F
promotion gate is to *diff* the engine's decisions against the live alerts on
demo, not to trade the setup twice. There was no safe way to run the two side
by side; shadow mode is it.

### What changed

- New signed field **`TradePlan.shadow: bool`** (`core/src/trade_plan.rs`,
  `#[serde(default)]` → live for plans registered before the field existed).
  It rides the existing whole-body HMAC, so a plan's shadow/live status is
  fixed at arm time and can't be flipped in flight.
- The cron engine (`src/cron/engine.rs`) honours it: a shadow plan is evaluated
  and its `PlanState` advanced **identically** to a live plan (same candles,
  same FSM, same watermark), but each fired intent is logged as a
  `cron engine SHADOW would-fire:` line instead of being dispatched — no broker
  order, no seen-id mark.
- `tv-arm` gains **`--shadow`** (`tv-arm/src/args.rs`), threaded through
  `register_trade_plan` → `build_trade_plan` so `--register-plan --shadow`
  registers an observe-only plan. The arm-time `info!` log now reports
  `shadow=…`.

### Breaking

- `tv_arm::trade_plan_build::build_trade_plan` gains a trailing `shadow: bool`
  parameter. Internal to this repo; the only caller is the tv-arm pipeline.

### Config

- New CLI flag `tv-arm --shadow` (default off → live). Only meaningful with
  `--register-plan`.

### Tests

- `core`: `shadow_flag_round_trips`, `missing_shadow_defaults_to_live`.
- `tv-arm`: `shadow_flag_carried_onto_plan`, plus the existing builder tests
  assert the default build is live.

### Follow-up

- Run a demo setup with `--register-plan --shadow` beside the live TV alerts
  and diff the `SHADOW would-fire` log lines against the alerts' actual
  placements — the empirical Stage F gate. This also produces the recorded-fire
  dataset the H&S historical-replay parity follow-up needs.

## v30 — 2026-06-17 — H&S Pine candle detector ported to Rust (server-side `PinePattern`, Stage E)

### Why

The H&S `05-enter` was the last condition still evaluated on TradingView's
servers: it fired on the paid "Long/Short Pattern" alertconditions of the
`candle-signals-v2.pine` detector. To evaluate H&S entries in the server-side
engine (and drop the runtime TV dependency for H&S, like M/W already has), the
detector is ported to Rust.

### What changed

- New `core/src/signals/` module — a faithful port of `candle-signals-v2.pine`:
  per-candle metrics, Wilder ATR with the timeframe-dependent length, the five
  pattern detectors (pinbar / tweezer / double-tweezer / regular- &
  floating-engulfer) with the Pine priority order and signal geometry, and the
  pending→valid→invalid state machine (confirmation latch, opposing-signal
  invalidation with golden-protect, recent_high/low lookback). The public seam
  is `latched_signal_at(window, as_of, cfg) -> LatchedSignal`.
- The engine's `evaluate_plan` gains a `detector_window`; `Trigger::PinePattern`
  is now evaluated (was a Stage-D stub) over that window, gated by direction +
  optional pattern kind. A fired H&S enter carries the latched signal geometry
  onto its shell via the new `Shell::from_candle_and_signal`, so it resolves
  entry/SL/TP against the *pattern* extremes (the bug-010 `SignalHigh`/
  `SignalLow` anchors) exactly as the TV alert's `{{plot(...)}}` substitutions
  did.
- `src/cron/engine.rs` fetches a wider detector back-window for Pine plans.

### Behaviour

The engine now evaluates **both** M/W and H&S server-side, in parallel with the
TV alerts (no change to existing trades), on the `*/15` tick — until proven on
demo (Stage F retires the alerts).

### Intentional divergence (bug #10B)

The port confirms a signal only on a **fully-closed** pushing bar (the engine
never sees an unclosed bar), fixing the Pine one-bar-early confirm timing (the
ADIDAS 5:30-vs-5:45 case). The historical-replay parity check will show this
diff against recorded Pine fires.

### Tests

- `core/src/signals/` — metrics, ATR, each detector, the state machine
  (confirm / breach-unconfirm / late-push / recent-extremes).
- `engine/src/evaluate.rs` — Pine entry fires with geometry, wrong-direction
  block, kind filter, retest gate.
- `core/src/intent.rs` — `from_candle_and_signal` folds geometry; the
  `SignalHigh`/`SignalLow` anchors resolve to the pattern extremes.
- core 498 / engine 28 / worker 199 green; clippy + fmt + wasm32 clean.

### Follow-up

Historical-replay parity: replay candle history through the Rust detector and
diff fires + geometry against recorded Pine fires. Needs the recorded-fire
dataset assembled first.

## v29 — 2026-06-17 — H&S/IHS enter anchors entry+SL to signal_high/signal_low (bug #10 finding A)

### Why

An H&S `enter` fires twice — once on the break candle (`signal_confirmed: 0`)
and once on the confirmed re-fire (`signal_confirmed: 1`). A confirmed re-fire
is meant to be the *same trade* — same pattern-invalidation stop — just
confirmed a candle later. Instead it silently became a *different,
tighter-stopped* trade: the worker anchored both entry and SL to the
**triggering candle's own high/low**, so the narrower confirmed candle handed a
tighter, drifted stop. Surfaced by `hs-adidas-b70c1d31` (ADIDAS short,
2026-06-16): designed entry 174.0 / SL 175.62 (stop 1.62) became entry 173.30 /
SL 174.30 (stop 1.00) ≈ the confirmed candle's own low/high — even though
`signal_high 175.61` / `signal_low 173.99` were identical on both fires. The
re-substituted trade would have stopped out near-instantly had it filled, and
it corrupts attribution (recorder's SL ≠ broker's SL).

### What changed (behaviour)

- New `PriceAnchor::SignalHigh` / `PriceAnchor::SignalLow` variants resolve to
  the shell's latched `signal_high` / `signal_low` (with the same graceful
  `unwrap_or(high/low)` fallback as the `recent_*` anchors).
- The H&S / IHS enter builders now anchor entry **and** SL to those signal
  extremes instead of the candle wick: H&S short = entry `signal_low`, SL
  `signal_high`; IHS long = entry `signal_high`, SL `signal_low`. The
  break-candle fire and the confirmed re-fire now resolve to identical
  geometry.
- `sl_anchor` override now also accepts `signal_high` (short) / `signal_low`
  (long).

### Breaking

None. Additive enum variant — existing intents using `from: high`/`low`/etc.
still resolve exactly as before.

### Tests

- `core`: `anchor_price` unit tests for `SignalHigh`/`SignalLow` (present +
  fallback + YAML round-trip); a resolution regression
  (`hs_short_signal_anchored_enter_resolves_identically_across_refires`) using
  the real adidas numbers, asserting entry+SL are identical across the
  break-candle and confirmed-candle shells.
- `cli`: H&S/IHS builder geometry tests updated to assert the signal anchors.

### Follow-up

Finding B of bug #10 (Pine emitted `signal_confirmed: 1` one candle too early)
is a separate Pine-source fix, not in this change.

## v28 — 2026-06-16 — expired/too-early intents return 200 declined, not 400 (bug #9)

### Why

A well-formed, correctly-signed intent that arrives after its `not_after`
(expired) or before its `not_before` (too early) is the *expected*
end-of-life outcome for any scheduled TradingView alert that keeps firing
past its intent's lifetime. The worker mapped **all seven** `IncomingError`
variants to a single `400 "rejected"`, so a routine stale fire read as an
HTTP 400 bad request — indistinguishable from a genuinely malformed/forged
request (bad YAML, bad HMAC sig, unsupported version, malformed `trade_id`).
This polluted the `trading-tax-tracker` timeline/verdict and masked real
bad-body / forgery defects in the 4xx noise. Surfaced by `m-aud-usd-007dfa5e`
on 2026-06-16. Same status-code-conflation defect as bug #7 (v27), here at
the `parse_and_verify` gate rather than the `resolve` gate.

### What changed (behaviour)

- **New `IncomingError::disposition()`** → `IncomingDisposition`
  (`DeclinedExpired` / `DeclinedTooEarly` / `Rejected`), a pure
  (KV-free, clock-free) classifier. `Expired`/`TooEarly` are benign 200
  declines; **every** other variant — including `StaleShellTime` (a >24h-old
  plaintext `time` smells of replay) — stays a 400 reject.
- The `parse_and_verify` match site in `src/lib.rs` now matches on
  `err.disposition()`: `Expired` → `200 "declined: intent-expired"`,
  `TooEarly` → `200 "declined: intent-too-early"` (logged at info via
  `rlog!`), all others → unchanged `400 "rejected"` (`rlog_err!`).

### Breaking

None. New public `IncomingDisposition` enum + `IncomingError::disposition()`
method; existing variants and `Display` unchanged.

### Tests

- `disposition_splits_time_window_from_bad_request` — `Expired`/`TooEarly`
  classify as their declined dispositions; the five bad-request variants
  classify as `Rejected`.
- `disposition_stale_shell_time_is_rejected_not_benign` — `StaleShellTime`
  is explicitly **not** folded in with the benign declines.

### Follow-up

Not yet deployed to staging — bakes on `main` first per the
develop-on-main / let-it-bake-on-staging split.

## v27 — 2026-06-15 — M/W not-armed-yet declines are 200, not 400 (bug #7)

### Why

Every M/W `enter` bar that isn't yet a valid arming bar was declined with
`ResolveError::InvalidGeometry`, and the worker mapped **all** resolve errors
to a single `400 rejected: resolve-failed`. So a routine "decline this bar,
stay armed for the next" — the *most common* M/W enter outcome — read as an
HTTP 400 bad request, indistinguishable from a genuinely malformed enter
(wrong-side SL, entry outside SL..TP, sub-1R, bad script). This polluted the
`trading-tax-tracker` timeline/verdict and masked real geometry bugs in the
noise of routine declines. Surfaced by `m-japan-225-ccabdfb7` on 2026-06-15.

### What changed (behaviour)

- **New `ResolveError::NotArmedYet`** variant. The three M/W arming gates in
  `from_mw_intent` (right-tower confirmation, middle-of-the-M cross, breakout
  stop on the correct side of the close) now return `NotArmedYet` instead of
  `InvalidGeometry`.
- **Worker maps it to a benign `200 declined: mw-not-armed`** (distinct
  `outcome` string), while genuinely malformed enters keep `400
  rejected: resolve-failed`. The decline is still a seen-id no-op, so the
  setup stays armed for the next bar exactly as before — only the wire status
  and outcome string change.

### Breaking

None. `InvalidGeometry` retains its bad-request meaning for the standard
(non-M/W) wrong-side SL/limit/stop cases. No wire-format or signed-field
change.

### Tests

- `core`: the nine M/W gate-decline tests now assert `NotArmedYet`; added
  `all_three_arming_gates_return_not_armed_yet` pinning all three gates to the
  new variant (bug #7).
- The standard-path wrong-side tests still assert `InvalidGeometry`,
  preserving the distinction at the `lib.rs` match site.

### Follow-up

Pairs with bug #8 (`trading-tax-tracker` timeline drops the `mw-abort`
veto-set event) — the timeline side consumes this 200/400 split to stop
labelling routine declines as bad requests.

## v26 — 2026-06-15 — M/W overshoot veto (180% of top→neckline)

### Why

An M/W entry that triggers after price has already run most of the way to TP
has poor R:R — the projected move is nearly done. H&S already guards this with
the `pcl-exhausted` veto; M/W had no equivalent. Operator request: veto if any
low (M) / high (W) reaches **180% of the top→neckline leg** at any point
(except for an already-open position).

### What changed (behaviour)

- **New `01-veto-mw-overshoot` alert** in the M/W bundle (now five alerts:
  cancel, abort, **overshoot**, trade-expiry, enter). A `price crosses` alert
  at the **180% of top→neckline** level — `top − 1.8·(top − neckline)`, which
  equals `neckline − 0.8·(top − neckline)` (0.8 legs past the neckline toward
  TP). Fires intra-bar (`OnFirstFire`); the `05-enter` lists `mw-overshoot` in
  its `vetos`.
- **`CancelPending`** — cancels a pending stop + blocks future entries, never
  closes an open position (entry-gate, not thesis invalidation).
- **Static, safe-direction.** The level is baked at arm time. Pine can't move
  an alert and the WASM worker can't re-issue one, so as the pattern grows a
  higher right shoulder / lower neckline the baked level only fires *early* —
  over-vetoing (blocks some valid late entries, never lets a genuinely overshot
  trade through). No worker-side live re-arming (deferred).

### Config

- New veto name `mw-overshoot` (`MW_OVERSHOOT_VETO_NAME`, single source of
  truth). New basename `01-veto-mw-overshoot` (`AlertBasename::VetoMwOvershoot`).
- No wire-format change (contract unchanged): it's another `veto` intent +
  another chart price alert, both already-supported shapes.

### Tests

- `mw_geometry::overshoot_level` M/W worked examples + 180%-from-top /
  0.8-legs-past-neckline equivalences.
- `alert_spec`: overshoot is a `PriceValue` at 1.1056 (M worked anchors),
  `Cross` / `OnFirstFire`; without-path returns `None`.
- conventions basename round-trip (16→17 variants) + literal.
- cli bundle: five alerts in order, all three price vetos `CancelPending`,
  enter `vetos` includes `mw-overshoot`.

### Follow-up

- Worker-side live recomputation of the level (chase the moving geometry) —
  needs the worker to re-issue chart alerts, which it can't today (WASM, no TV
  creds). Only if static over-vetoing proves painful in practice.

## v25 — 2026-06-15 — M/W dynamic geometry: live right-shoulder / neckline + rogue-wick + candle `open`

### Why

The book reads the higher shoulder and the deepest neckline off a *finished*
chart. We arm with only the left shoulder + neckline known and the right
tower still forming, so the worker must recover those two facts live. v24
fixed *when* to arm; this fixes the *geometry* it arms with.

### What changed (behaviour)

- **Candle `open` threaded through the shell** (Phase B0). `Shell.open:
  Option<f64>`; added to `sig::UNSIGNED_VALUE_KEYS`, the `incoming` shell-key
  whitelist, the CLI TV-template body (`open: {{open}}`), the Rhai scope, and
  Pine `candle-signals-v2` v2.5's `Every Bar Close` message. Optional →
  backward-compatible; old bodies verify unchanged.
- **`mw-state:<scope>:<trade_id>` KV keyspace** (Phase B1): `MwState`
  (revised neckline + recorded right shoulder) with get/upsert/clear.
- **`plan_mw_update` / `effective_mw_params`** (Phase B2, pure): per-bar
  decision over the prior state + the bar's **body** extremes —
  - higher right shoulder → SL anchor (higher of the two shoulders for M);
  - deeper body still ≥ 60% of the runup→shoulder leg → revise the neckline;
  - body past the 60% floor → cancel;
  - all body-based, so a rogue wick can't move geometry or cancel.
- **Wired into `run_enter`** (Phase B3): `maybe_update_mw_state` reads/updates
  KV, then resolves the bar against the effective params. On cancel it cancels
  pending + writes a trade-scoped `mw-cancel` veto (`MW_CANCEL_VETO_NAME`, new
  shared const) and **never closes an open position**.

### Breaking

- `Resolved::from_mw_intent` is now `pub` (worker passes effective params).
  New `MW_CANCEL_VETO_NAME` const (CLI enter-builder + worker share it).
  No wire-format break — `open` is optional; contract stays `v3`.

### Config

- Pine must be **republished** to v2.5 for charts to start sending `open`
  (the dynamic update is a no-op until then). New KV keyspace needs no
  config — the existing `TRADE_CONTROL_KV` binding covers it.

### Tests

- core: `plan_mw_update` (cancel / floor / rogue-wick-doesn't-cancel /
  right-shoulder record / neckline revise / W mirror), `effective_mw_params`,
  `body_high`/`body_low`, MwState memstore round-trip, `open` sig round-trip.
- The `maybe_update_mw_state` glue (KV read → plan → write/cancel) is thin
  and verified by dev-deploy replay rather than a native mock (the worker's
  `run_enter` needs a Cloudflare `Env`; the decision logic it calls is
  fully covered in core).

### Follow-up

- `incoming`'s shell-key whitelist duplicates `sig::UNSIGNED_VALUE_KEYS` —
  a future refactor could derive one from the other (drift bit B0 once).

## v24 — 2026-06-15 — M/W real-time arming: right-tower window + "middle of the M" downward cross

### Why

M/W setups arm in **real time**, when only the left shoulder (B) and
neckline (C) are printed — the right tower hasn't formed yet. The strategy
book is the opposite: a **post-hoc** method that stops at the neckline once
*both* towers are complete ("no retest required"). Applying the post-hoc
rule live is what armed premature entries. v16 added a first guard (the
0.7→1.3 second-peak window); this completes the real-time arming by also
requiring price to **roll back off** the confirmed right tower before the
breakout stop arms.

### What changed (behaviour)

- **`Resolved::from_mw_intent` (`core/src/intent/mw_resolution.rs`)** now
  gates the per-bar enter on **two** confirmations, both MID-price on the
  neckline→peak (C→B) leg:
  1. **Right-tower window** (unchanged math, reframed): the bar's extreme
     (high for M, low for W) must reach within 30% of the left-shoulder high
     — `[neckline + 0.7×(peak−neckline), neckline + 1.3×(peak−neckline))`.
  2. **"Middle of the M" downward-cross trigger** (new): the bar must cross
     back through `mid50 = neckline + 0.5×(peak−neckline)`. M (short):
     `high ≥ mid50 AND close < mid50`; W (long): `low ≤ mid50 AND
     close > mid50`. A bar that hasn't crossed is declined → stay armed.
- Entry/SL/TP price math (mid→bid/ask, exactly 1R TP) is **unchanged**; the
  fill is still a breakout stop at the neckline. Non-`Ok` resolves still
  don't mark the intent seen, so the setup stays armed across bars.

### Breaking

- Constant `SECOND_PEAK_MIN_FRAC` renamed to `RIGHT_TOWER_MIN_FRAC`; added
  `MID_CROSS_FRAC = 0.5`. Internal only — no wire-format or CLI change.

### Config

- None. No new intent fields, no contract bump (`v3` unchanged) — the gate
  is worker-internal on the existing `mw:` enter.

### Tests

- New `mw_resolution` tests: right tower confirmed but not crossed (M and W)
  → declined; crossed → armed (M and W); `close == mid50` boundary →
  declined. Existing worked-example + AUD/CAD tests still pass (their shells
  already cross mid50). 436 core tests green.

### Follow-up

- Phase B (planned): KV-backed dynamic neckline/right-shoulder recording
  (higher right shoulder → SL anchor; deeper body-low ≥60% revises neckline;
  <60% cancels) + body-based rogue-wick handling.

## v22 — 2026-06-13 — spread-blackout System 3: cancel resting entry orders on blackout, re-drive on recovery

### Why

Sub-plan 5 (the **last**) of the DST-aware spread-blackout feature, and the
one that **actually fixes the motivating trade**: a resting stop-entry that
sat through the post-NY-close liquidity trough filled into the spread
blowout and stopped out instantly (~−1.38R, almost all spread). System 3
cancels resting **entry** orders during the blackout and re-drives the exact
same entry once the spread recovers — routing an overrun stop to the
`on_too_close` fallback (v17) and dropping a stale limit. Builds on v17
(`on_too_close`), v18 (`get_quote`/`list_pending_orders`/`cancel_order`),
v19 (record + crons + reserved `cancelled_orders`), v21 (Cron 1 widen + Cron
2 restore, which this extends rather than duplicates).

### What changed (behaviour)

- **Cron 1 (apply edge), `src/cron/blackout_cancel.rs` (new):** after the
  System-2 widen, on the same affected-account scan, `list_pending_orders`
  for each account; for each resting entry order whose **instrument spread is
  elevated** (sampled via `get_quote`), store a `CancelledOrder` (id + whole
  signed body) onto the per-trade `SpreadBlackoutRecord` **then**
  `cancel_order` (store-before-cancel crash-safety). An order with no stored
  signed body is **never cancelled** (can't be restored ⇒ don't strand it).
- **Cron 2 (recovery), `src/cron/blackout_restore.rs` (new):** for each
  `CancelledOrder`, reconstruct an authentic `Verified` from the stored
  signed body via `incoming::parse_and_verify` (same signing key the HTTP
  path uses), pre-check the fill-side recreate geometry, and **re-drive
  through `run_enter`** so sizing/gates/`on_too_close` all apply. Runs at
  both the recovery and backstop clear points, alongside the System-2 stop
  restore, on the same record. Expired-window bodies are dropped, not placed.
- **Recreate geometry (`core/src/blackout_recreate.rs`, new):** pure
  `recreate_stop` / `recreate_limit` predicates (FILL-SIDE bid/ask, not mid)
  + a `restore_plan` branch decision, fully truth-tabled.
- **New entry-path KV write:** every successful single-shot placement now
  writes an `order:<broker_order_id>` row holding the raw signed body, TTL'd
  to the alert window. This is the only place the original signed bytes
  survive long enough for the apply cron to recover them.

### Breaking

- `run_enter` gains a `raw_body: Option<&str>` parameter (HTTP path passes
  the request body; the cron re-drive passes the stored body). `run_action`
  gains `raw_body: &str`. `ActionResult` and `run_enter` are now
  `pub(crate)` so the cron can re-drive. No wire-format change.

### Config / secrets

- No new secrets. The cron re-uses the existing `SIGNING_KEY` to re-verify
  stored bodies (factored into `signing_key(env)`).

### Tests

- `core` (`blackout_recreate`): 19 unit tests — four-kind × recreate
  true/false table, swapped entry/tp guard rows (the sign-bug canary),
  boundary equality, fill-side discrimination (long reads ask / short reads
  bid), and the full `restore_plan` branch matrix.
- worker (`blackout_cancel`): 4 unit tests — pure record-merge (fresh +
  existing-record push, Sub-plan-4 `original_stops` coexistence, same-id
  de-dup on re-fire, pip backfill).
- Native + wasm + cli all build; clippy clean on native and wasm; fmt clean.

### Follow-up (still open)

- **Demo-confirm** the cancel + re-drive on `reversals` before live (dry-run
  → demo). Not yet exercised against a real broker.
- **Multi-shot re-drive retry-slot:** a re-drive of a *multi-shot* cancelled
  order can still consume a `max_retries` slot (single-shot is unaffected).
  The fix is a `restoring` flag into `record_placement`; deferred.
- `on_too_close: limit` still degrades to `skip` (v17 carry-over); an overrun
  stop with `action: limit` skips-and-stays-retryable.

## v21 — 2026-06-13 — spread-blackout System 2: widen open stops on blackout, restore on recovery

### Why

Sub-plan 4 of the DST-aware spread-blackout feature (builds on the v18
broker-trait `amend_stop`/`list_open_positions`, the v19 window marker +
per-trade record, and the v20 entry-reject). v19 left the widen/restore as
flag-lifecycle stubs. This lands **System 2**: protect an *already-open*
position from the post-NY-close spread blowout by widening its stop away
from price at the window edge and restoring it to the exact original after.
The motivating trade (`hs-eur-nzd-c1e0f25b`, EUR/NZD short) stopped out for
~−1.38R, almost all of it spread — its stop sat right where the blown-out
ask clipped it.

### What changed

- **Pure widen helpers** (`src/cron/blackout_widen.rs`, new): `widened_stop`
  (SHORT → SL up, LONG → SL down; the sign-bug seam with a direction-matrix
  + pip-scaling test) and `clamp_widen` (the 22–40-pip clamp), with
  `WIDEN_FLOOR_PIPS`/`WIDEN_CEIL_PIPS` consts. KV-free, native unit-tested.
- **Cron 1 widen** (`src/cron/blackout_apply.rs`): after opening the window
  marker, list open positions per affected account (sourced from the
  `EntryAttempt` rows), join each to its originating attempt (by
  `position_id → broker_trade_id`, fallback `instrument+direction+account`),
  guard on the record's `applied` flag (idempotent — no double-widen),
  **record the original SL first then amend** (crash-safe), and bake
  `pip_size` onto the record. Pure `join_position_to_attempt` helper +
  tests. Logs an `INTENT amend_stop …` line before every amend
  (precondition read-back).
- **Cron 2 restore** (`src/cron/blackout_watch.rs`): at both clear points
  (spread-recovered AND backstop), restore each remembered stop to its
  original **verbatim** (never `current − widen`) before clearing. Closed
  position (`NotFound`) is benign; a failed restore is logged loudly and the
  record still clears.
- **Units reconciliation (cross-sub-plan fix):** the cron side previously
  compared spread in absolute price while System 1 worked in pips. Added
  `pip_size` to `SpreadBlackoutRecord` (baked at apply time from the joined
  `EntryAttempt`) and `pip_size: Option<f64>` to `EntryAttempt` (snapshotted
  from `Intent.pip_size` at placement). `blackout_watch` now converts
  `ask − bid` to pips via the record's pip. The elevated (8p) and recovered
  (4p) cutoffs are unified in `src/spread_blackout.rs` with the hysteresis
  invariant `recovered < elevated`.

### Breaking

None on the wire. KV: `SpreadBlackoutRecord` gains `pip_size`, `EntryAttempt`
gains `pip_size` — both `#[serde(default)]`, so older rows decode (pip
`0.0`/`None` ⇒ the cron skips the widen / falls back to backstop-only clear,
never widens with a wrong pip).

### Config

`WIDEN_FLOOR_PIPS = 22.0`, `WIDEN_CEIL_PIPS = 40.0` (flat, per the
self-scoping argument — majors never trip the elevated sample). The
elevated/recovered spread cutoffs (`SPREAD_BLACKOUT_ELEVATED_PIPS = 8.0`,
`SPREAD_BLACKOUT_RECOVERED_PIPS = 4.0`) are co-located in
`src/spread_blackout.rs` and provisional — calibrate on demo.

### Precondition (not yet cleared)

`amend_stop` on an OPEN position via TradeNation's `AmendCloseOrder` is
UNVERIFIED (zero upstream callers). **Live widening must not be trusted
until demo-confirmed** on `reversals` (open a position, amend the SL, read
it back, confirm SL moved + TP unchanged). The apply cron logs every
intended amend prominently for the read-back. See `TODO.md`.

### Tests

`blackout_widen`: 7 (direction matrix incl. wrong-direction sign guard,
pip-scaling FX/index/JPY, clamp floor/in-band/ceiling/boundaries).
`blackout_apply`: 4 join tests (broker_trade_id-first, fallback,
miss, account-scope). `blackout_watch`: pips-units recovery + `spread_in_pips`
(unusable-pip → INFINITY). `spread_blackout`: hysteresis invariant.
`core`: `SpreadBlackoutRecord`/`EntryAttempt` serde round-trip + old-row
default decode. Worker 179, core 412, cli 233 — all green; native + wasm +
cli build clean; clippy clean both targets.

## v20 — 2026-06-13 — spread-blackout System 1: reject new entries during the window

### Why

Sub-plan 3 of the DST-aware spread-blackout feature (builds on the v18
broker-trait `get_quote` and the v19 global window marker). The window
marker armed in v19 had no consumer yet. This lands **System 1**: the
"don't open a new position during the post-NY-close liquidity trough"
half. A real trade (`hs-eur-nzd-c1e0f25b`, EUR/NZD short) entered
straight into a ~20p blowout and stopped out for ~−1.38R, almost all of
it spread, not a real price move — exactly the case this rejects.

### What changed

- **Pure decision helper** (`src/spread_blackout.rs`, new):
  `spread_blackout_decision(window_open, spread_pips, threshold_pips) -> bool`
  (strictly `>`, so exactly-at-threshold passes), the threshold lookup
  `elevated_threshold_pips(instrument)`, and the provisional constant
  `SPREAD_BLACKOUT_ELEVATED_PIPS = 8.0`. KV/broker-free, native unit-tested.
- **Entry wrapper** (`src/lib.rs`, `run_enter`): at the very end of entry
  processing — after every gate and `Resolved::from_intent`, immediately
  before the broker `EntryRequest` — read the global window marker. If
  open, sample the live spread (`Broker::get_quote`, `ask − bid` ÷
  `pip_size`) for the incoming instrument and reject when elevated.
  - **Outcome:** `rejected: spread-blackout`, **HTTP 423 Locked**
    (mirrors the pause / cooldown / news transient-state-block family).
  - **No instrument classification** — the live spread sample *is* the
    filter; majors pass, blown-out thin crosses reject, fine days don't
    black out at all.
  - **Reject, NOT delay** — no KV write, no re-fire queued; the next
    signal bar re-runs the check.
  - **Does NOT poison the seen-id** — `ActionResult::Rejected` is a `Skip`
    in `seen_decision` (no `mark_seen`); the next fire is allowed through.
  - **Fail-open** on a window-marker read error OR a `get_quote` error at
    decision time (logs `console_error!`, allows the entry) — a transient
    hiccup must never block a legitimate trade.
  - **Window closed = no broker round-trip** (no `get_quote` on the
    overwhelmingly-common path).

### Breaking

None. No new wire field, no new KV namespace (consumes v19's marker), no
new secret.

### Config

The elevated cutoff is a provisional single constant
(`SPREAD_BLACKOUT_ELEVATED_PIPS`, 8 pips). It and v19's recovery cutoff
(`blackout_watch::recovered_cutoff`) are the **same open question** and
must be calibrated together (elevated > recovered, for hysteresis; units
currently differ — see the `TODO(open-question)` in both modules).

### Tests

Five new native unit tests on the pure helper: window-closed → pass,
window-open + wide → reject, window-open + tight → pass, boundary
(exactly-at-threshold → pass), threshold-lookup returns the constant for
any instrument. Native + wasm builds clean; clippy clean.

### Follow-up

Threshold calibration on demo (the open question); fail-closed variant
if the trough also degrades the quote endpoint; Sub-plans 4/5 (widen
open stops / cancel resting orders).

## v19 — 2026-06-13 — spread-blackout state + crons skeleton (no entry-reject/widen/cancel yet)

### Why

Sub-plan 2 of the DST-aware spread-blackout feature (builds on the v18
broker-trait foundations). Right after New York's 17:00 close a ~1h
liquidity trough blows the spread out on thin FX crosses (a real trade,
`hs-eur-nzd-c1e0f25b`, stopped out for ~−1.38R almost entirely on
spread). This lands the **state machine + cron skeleton** the rest of
the feature hangs off — it does **not** reject entries (sub-plan 3),
widen stops (sub-plan 4), or cancel/restore orders (sub-plan 5).

### What changed

- **DST module** (`core/src/ny_clock.rs`, new): hand-rolled US Eastern
  DST rule (2nd Sun Mar → 1st Sun Nov), KV/clock-free pure fns
  `is_ny_close_edge(now)` and `ny_is_edt(date)`. No `chrono-tz` (keeps
  the WASM bundle small). NY close = 21:00 UTC under EDT, 22:00 UTC
  under EST. Full proven-fixture-table unit tests + DST-boundary
  exactness.
- **KV state** (`core/src/state.rs`, `src/state/kv.rs`): two new kinds
  under the `spread-blackout:` namespace — the singleton global window
  marker `spread-blackout:window` (`SpreadBlackoutWindow`) and the
  per-trade record `spread-blackout:rec:<trade_id>`
  (`SpreadBlackoutRecord`). Six new `StateStore` methods (set/get
  window, upsert/get/list-all/clear record). `original_stops` /
  `cancelled_orders` (+ `RememberedStop` / `CancelledOrder`) are
  **reserved** for sub-plans 4/5 and empty for now. Surfaced in the
  `status` `Snapshot` (`spread_blackouts` + `spread_blackout_window`).
- **Crons** (`wrangler.toml`, `src/cron.rs`): a second + third daily
  cron added to the flat `crons` array (`5 21` and `5 22 * * *`, both
  DST candidate minutes); `scheduled` now dispatches on `event.cron()`.
  **Cron 1** (`src/cron/blackout_apply.rs`) opens the window marker when
  `is_ny_close_edge(now)`. **Cron 2** (the 15-min job) gains the
  **recovery watcher** (`src/cron/blackout_watch.rs`): for each
  `applied` record, clear on spread-recovery (live `get_quote`) or the
  ~3h backstop. Three safety rules (hard restore floor / backstop
  timeout / never-touch-what-you-didn't-apply) coded + unit-tested as
  pure predicates. `acquire_broker_for_account` / `open_store` /
  `BrokerHandle` factored out of `sweep.rs` for reuse.
- **`BLACKOUT_BACKSTOP_SECONDS`** (`src/cron/constants.rs`, ~3h): single
  source of truth for the window TTL, the record TTL, and the watcher
  backstop so they can't drift.

### Breaking

- `Snapshot` is no longer `Eq` (it now carries `f64` stop prices via
  `SpreadBlackoutRecord`); still `PartialEq`.
- `StateStore` gains six methods — every impl (`KvStateStore`, the
  test stores `MemStateStore` / `CountingStore` / `SeenSpyStore`) was
  updated.

### Config

- `wrangler.toml` `crons` array gains `5 21 * * *` and `5 22 * * *`.
  Kept the flat-array form (the `[[triggers.crons]]` double-wrap-bug
  comment is preserved).

### Open question (recorded, not resolved)

- The spread *recovered* / *elevated* thresholds and the pip-size source
  for a cron-sampled instrument (the watcher has no intent in hand) are
  left as a coarse placeholder constant with a `TODO(open-question)` in
  `src/cron/blackout_watch.rs`. Sub-plan 3 inherits the same question
  for the entry-reject side.

### Tests

- `core`: 412 pass (+14: 11 `ny_clock` fixtures + 3 state serde
  round-trips).
- worker: 161 pass (+5 `blackout_watch` pure-predicate tests; +4 kv
  decode tests for the new entry types).

### Follow-up

- Sub-plan 3: entry-reject reading the window marker.
- Sub-plans 4/5: populate `applied` / `original_stops` /
  `cancelled_orders`; restore stops/orders at the marked watcher points.
- Resolve the spread-threshold + pip-source open question.

## v18 — 2026-06-13 — broker-trait spread/positions/amend foundations (no behaviour change)

### Why

Sub-plan 1 of the DST-aware spread-blackout feature. The blackout systems
need four broker capabilities the `Broker` trait didn't expose: the live
bid/ask **spread**, **list open positions** (to widen their stops),
**amend a stop** (widen + restore), and **list pending orders** (cancel +
restore). All four already exist one layer down (`tradenation-api`,
`oanda-client`); this surfaces them through the trait with **zero
behaviour change** — no worker action calls the new methods yet.

### What changed

- **New trait surface** (`core/src/broker.rs`): types `Quote { bid, ask }`
  (with `mid()` / `spread()`), `OpenPosition`, `PendingOrder`, and
  `AmendError` (modelled on `CancelError`, plus a `NotFound` variant);
  methods `get_quote`, `list_open_positions`, `amend_stop`,
  `list_pending_orders`. `get_current_price` becomes a **default method** =
  `get_quote().mid()`, so the mid logic lives once.
- **TradeNation adapter** (`src/tradenation_adapter.rs`): `get_quote` is the
  old `get_current_price` minus the `/2.0` (it was discarding the spread);
  the three new methods go through `get_account_details` / `amend_order`.
  Pure mapping fns `tn_position_to_open` / `tn_order_to_pending` /
  `find_amend_target` are split out and unit-tested.
- **OANDA** (`broker-oanda/src/oanda.rs` + `lib.rs`): full parity —
  `get_quote` via the pricing endpoint (`best_bid`/`best_ask`),
  `list_open_positions` via `get_trades`, `amend_stop` via
  `modify_trade_stops`, `list_pending_orders` via `get_pending_orders`.
  `oanda_trade_to_open` / `oanda_order_to_pending` are pure + unit-tested.
- **MockBroker** (`src/retry_gate.rs`, test-only): the three list/amend
  methods are `unimplemented!()` (unused by retry-gate tests);
  `get_quote` returns `Transient` (preserving the old behaviour the
  default `get_current_price` now inherits).

### Breaking

- Trait-level: `Broker` gains four required methods and `get_current_price`
  is now a defaulted method. Any external `impl Broker` must add the new
  methods. All three in-repo impls updated.

### Semantics gotchas preserved

- **`PendingOrder.trigger` is the entry trigger, NOT the SL/TP.** On
  TradeNation a pending entry order reports its trigger in
  `stop_order_price` / `limit_order_price`; the real SL/TP live in unparsed
  `IDO*` fields. The mapping labels it `trigger` with `is_stop`, never a
  stop-loss.
- **`amend_stop` on TradeNation is UNVERIFIED for open positions.** The
  upstream `amend_order` (`AmendCloseOrder`) has zero callers and it isn't
  confirmed it amends an *open position's* SL (keyed by the position's
  originating order id) vs only a resting entry order. Wired through with
  doc-comments flagging it; **sub-plan 4 must demo-confirm before any live
  widening.** A position with no take-profit passes `0.0` to the
  both-prices-required endpoint — also unverified whether the platform
  reads `0` as "no TP".

### Config / wire

- None. No new secrets, no new alert fields, no new outcome strings, no
  reconciliation impact.

### Tests

- `core`: `Quote::mid`/`spread` arithmetic; a mid-only mock proving the
  default `get_current_price` returns the quote mid.
- `tradenation_adapter`: Buy/Sell → direction, SL/TP optionality,
  trigger-or-skip for pending orders, `find_amend_target` (position by
  position_id / order_id, pending fallback, absent → None).
- `broker-oanda`: trade → open position (long/short, stake abs, SL/TP),
  pending order → `is_stop` mapping, non-entry / unparseable skip.

### Follow-up

- Sub-plan 4 demo-confirms TradeNation `amend_stop` on an open position
  (and the no-TP `0.0` semantics) before any live stop-widening.
- Sub-plans 2–5 wire these methods into the blackout systems.

## v17 — 2026-06-13 — `on_too_close` stop-entry fallback (`#19-10` recovery)

### Why

A stop-entry whose trigger has been overtaken by price (the breakout
happened in the gap between bar-close and the order resting) is rejected
by TradeNation with `#19-10` ("entry too close to / wrong side of
market"). Until now the worker (a) lost the error's identity — it
collapsed into the generic `OrderRejected` and surfaced as an opaque
`502 broker rejected the order` — and (b) had no recovery: the entry was
simply dropped. This is sub-plan 0 of the DST-aware spread-blackout
feature, which needs a "stop-can't-place → market / skip" fallback to
re-drive entries when it re-creates cancelled orders at the NY-close
edge.

### What changed

- **Distinct error, all three layers.** `tradenation_api` already
  classified `#19-10` as `TradeError::EntryTooCloseToMarket`;
  `broker-tradenation` (v0.9.0) now maps it to a new
  `EntryError::EntryTooCloseToMarket` instead of the catch-all, and
  `core::broker::EntryError` + `tradenation_adapter::from_upstream_error`
  carry it through. The worker renders the distinct outcome string
  `entry-failed: too-close-to-market` (still `ActionResult::Failed` →
  502, **no seen-id poison** — preserved so the next bar retries).
- **New wire field `on_too_close` on `EntrySpec::Stop`** —
  `{ action: market|limit|skip, max_slippage_pips: <n> }`. Default
  (omitted) = `skip`, byte-identical to pre-feature intents. `market`
  requires `max_slippage_pips` (validated). Resolved into
  `Resolved::on_too_close` (pips → price units) so the worker never
  re-reads pip size.
- **`action: market` recovery.** On a `#19-10` rejection the worker
  reads the current market price, applies the slippage guard, and — if
  within threshold — does **one** synchronous market re-place, re-sized
  against the actual fill reference. Out of threshold / `skip` /
  `limit` (unimplemented) / price-read failure all fall back to the
  terminal `Failed` (no poison). The re-place shares the multi-shot
  `EntryAttempt` slot — it does not consume a fresh one.

### Breaking

- `core::broker::EntryError` and `broker_tradenation::EntryError` each
  gain an `EntryTooCloseToMarket` variant (exhaustive matches must add
  an arm).
- `EntrySpec::Stop` gains an `on_too_close: Option<OnTooClose>` field
  (constructors must set it; `None` = today's behaviour).
- `Resolved` gains `on_too_close: Option<ResolvedOnTooClose>`.

### Config

- Worker pins `broker-tradenation` / `tradenation-api` to the new
  `broker-tradenation-v0.9.0` tag (which carries a transitive
  `time = "=0.3.41"` pin → `reqwest 0.12.23` in the lockfile).

### Tests

- broker-tradenation: `map_place_error` maps too-close distinctly.
- core: `on_too_close` parse / serialise round-trips, validation
  rejects `market` without `max_slippage_pips`, resolution carries the
  fallback and converts pips→price.
- worker: distinct outcome string classifies as Skip (no poison); the
  pure `too_close::market_replace_plan` slippage guard (within /
  out-of-threshold / short side / boundary / no-bound / non-finite).

### Follow-up

- `action: limit` re-place (sub-plan step 4) — currently degrades to
  skip; needs geometry validation so it doesn't create a `#19-9`.
- A `build-trade` / `tv-arm` CLI flag to opt a setup into `on_too_close`
  (the field is wired but no builder emits it yet).
- Demo verification per `dry_run_first_protocol`: craft a stop whose
  trigger sits behind current price on the TN demo and confirm the
  distinct log + market recovery / skip.

## v16 — 2026-06-13 — M/W second-peak confirmation window before arming

### Why

The M/W enter alert fires every bar close, and the worker armed the
breakout stop as soon as a bar merely *closed* on the entry side of the
neckline (`entry < close` for a short). It never looked at the bar's
high/low. On a real AUD/CAD demo setup (neckline 0.98339, peak 0.98509)
a bar closed just past the neckline with a high of only 0.98430 — short
of any real second peak — so the worker armed a sell stop at 0.983255
that later filled and stopped out. The book's rule is that price must
retrace back *into* the pattern far enough to form a genuine second
peak/trough before the breakout is valid.

### What changed

- `Resolved::from_mw_intent` now gates on a **second-peak confirmation
  window** before the existing stop-side check. The bar's extreme (high
  for an M, low for a W) must lie in `[min_retrace, cancel)` on the
  neckline→peak (C→B) leg:
  - `min_retrace = neckline + 0.7 × (peak − neckline)` — floor; a
    shallower poke past the neckline is declined (stay armed).
  - `cancel = neckline + 1.3 × (peak − neckline)` — ceiling; the same
    1.3 extension the `mw-cancel` veto guards, declined here as a safety
    net in case that veto hasn't fired. Upper bound exclusive.
- All comparisons are MID-price (neckline, peak and high/low are all
  mid) — no spread correction on this gate.
- Declines reuse `ResolveError::InvalidGeometry`, so (post the 2026-06
  seen-id fix) a declined bar does **not** mark the intent id seen — the
  setup stays armed for the next bar.

### Breaking

None. Pure tightening of the enter gate; intent wire format unchanged.

### Config

Two fixed worker constants, not signed fields:
`SECOND_PEAK_MIN_FRAC = 0.7` and `CANCEL_EXT_FRAC = 1.3` in
`core/src/intent/mw_resolution.rs`. Changing them needs a redeploy.

### Tests

5 new cases in `mw_resolution`: M high below floor declined (the AUD/CAD
regression), M high inside window armed, M high at/above cancel declined,
W low above floor declined, W low below cancel declined. Existing worked
M/W tests updated to pass explicit high/low (new `shell_hlc` helper).
385 core + 130 worker tests green.

### Follow-up

The `0.7` floor is currently a hardcoded constant shared by every M/W
setup. If a future setup wants a per-pattern floor, promote it to a
baked `MwParams` field (signed) the way `pip_size` is.

## v15 — 2026-06-13 — extend bug #6 hardening to per-key prefix listings

### Why

v14 made the array-blob index reads (`index:vetos` et al.) tolerant of one
bad legacy element. The *other* state reader — `list_json_with_prefix`, which
backs the `pause:` / `news:` listings read by `snapshot()` and
`list_pauses_for_trade` — still did a strict per-key `serde_json::from_str` and
bailed the **whole listing** with `?` on the first value that wouldn't decode.
Same latent failure mode as bug #6, just keyed-per-object instead of one shared
array. `PauseEntry` / `NewsEntry` haven't drifted yet, so it hadn't fired — but
the next required field added to either would have broken `status` and the
news-window close gate. Closed it now rather than wait for the incident.

### What changed

- `list_json_with_prefix` now decodes each listed value through a new pure
  `decode_keyed_value` helper that **drops and logs** (`kv list decode:
  dropping bad value key=… err=…`) any single value that won't deserialize,
  instead of failing the whole listing. A KV *I/O* error on a `get` is still
  fatal (genuine backend failure, not schema drift) — mirrors how `read_index`
  keeps the container-level error fatal.
- New native-safe `warn_dropped_keyed_value` shim alongside the v14
  `warn_dropped_index_element` (per-key listings identify the dropped record by
  key name, so no array index).

### Breaking

None. Pure robustness hardening; no API, wire-format, or config change.

### Config

None.

### Tests

Three new cases in `decode_index_tests`: a valid `PauseEntry` decodes; a legacy
`PauseEntry` missing required `blackout_id` is dropped (None, not fatal);
malformed JSON for one key is dropped, not propagated.

## v14 — 2026-06-13 — element-tolerant index decode (bug #6 fix)

### Why

On 2026-06-12 a single legacy-shaped element inside the `index:vetos` KV blob
was missing the required `trade_id` field. Because `set_veto` (and every other
index write) is a read-modify-write, the strict
`serde_json::from_str::<Vec<VetoEntry>>` in `read_index` failed on that one bad
element and took the *whole* array down. Result: **160 veto writes failed, 0
succeeded** across every account/instrument — no `mw-abort`, `mw-cancel`,
`too-high/too-low`, `trade-expiry`, or `close-on-reversal` veto could be
recorded, returning HTTP 500. A real pending short order (`26800323`, EUR/USD,
`reversals`) was never cancelled because its `mw-abort` 500'd four times.

### What changed

- `read_index` (the single generic chokepoint for **all five** index blobs —
  `vetos`, `seen`, `preps`, `cooldowns`, `prep-blocks`) now decodes
  **element-wise**: the blob is parsed as `Vec<serde_json::Value>` and each
  element is `from_value`'d into its struct individually. An element that fails
  to deserialize is **dropped and logged** (`index decode: dropping bad element
  key=… idx=… err=…`) instead of failing the read. The next `write_index`
  rewrites the blob without it (self-healing).
- A genuinely broken *container* (not a JSON array, truncated blob) is still a
  hard `StateError::Backend` — only element-level schema drift is tolerated.
- Logging uses the native-safe shim pattern (`worker::console_log!` on wasm,
  `tracing::warn!` off-wasm) so the decode stays unit-testable.

### Breaking

None. Pure robustness hardening; no API, wire-format, or config change.

### Config

None.

### Tests

New `decode_index_tests` module in `src/state/kv.rs`: a `trade_id`-less legacy
veto is dropped while the good one survives; all-valid blobs round-trip; empty
array stays empty; a non-array container is still fatal; and the same
drop-not-fatal behaviour is proven generic over `PrepEntry` (missing `step`).

### Follow-up

- `list_json_with_prefix` (news/pause keys, read by `snapshot()`) shares the
  same strict per-key decode and could be hardened the same way — **done in
  v15**.
- Operator: pending order `26800323` and any siblings on `reversals` were left
  live without veto protection during the 2026-06-12 window — reconcile
  open/pending orders against intended cancels manually.

## v13 — 2026-06-12 — experimental `veto_on_reversal` on reversal-close

### Why

A real setup got its `break-and-close` and `retest` preps, then price
reversed off a support line **before** the entry fired, and the trade
entered anyway and lost. The reversal-close machinery only flattens an
*open* position — fired before entry it's a no-op, so the entry sailed
through despite a strong "this trade won't work" signal. We want the same
reversal that would close the trade to optionally *veto the upcoming
entry* when it lands pre-entry.

### What changed

- New **opt-in, default-off** field `Intent.veto_on_reversal: bool`. On a
  price-windowed `close` (the reversal-close), when the close gate passes
  the worker also writes a `reversal` veto scoped to the intent's
  `trade_id`. A later `enter` for that setup then hits the existing
  `is_vetoed` gate and is rejected.
- Semantics are **StopNextEntry-style**: the veto only blocks future
  entries; it never force-closes a position beyond the close the intent
  already performs (consistent with "entry-gate vetos must not close
  positions"). Written on **every** gate-pass — pre-entry it blocks the
  entry; post-entry it harmlessly prevents a re-entry for the rest of the
  window. TTL = life of the alert window (`veto_ttl_seconds`).
- The worker reuses the existing `set_veto` / `is_vetoed` machinery — no
  new state primitive. The veto name is the fixed string `reversal`
  (`trade_control_core::intent::REVERSAL_VETO_NAME`, shared so the write
  side and the enter-builder can't drift).
- **Both halves move together.** The worker only checks veto names the
  `enter` lists in its `vetos`, so writing the veto is inert unless the
  matching `05-enter` also lists `reversal`. `build_trade_from_spec` adds
  `reversal` to the close's `veto_on_reversal` *and* to the enter's
  `vetos` whenever the flag is armed and `sr_reversal_ranges` is non-empty.
- CLI: `TradeSpec.veto_on_reversal` plumbs both halves, but only when
  `sr_bands` are present (a news-only reversal-close has no band to
  reverse off).
- tv-arm: new `--veto-on-reversal` flag (default off) sets it at arm time.

### Breaking

None. The field default-skips on serialize, so existing alerts are
byte-identical and in-flight bundles are unaffected.

### Config

- Intent wire: `veto_on_reversal: true` (optional, only on a
  price-windowed `close`).
- CLI spec: `veto_on_reversal: true` in `trade.yaml`.
- tv-arm: `--veto-on-reversal`.

### Validation

`veto_on_reversal` is rejected on a non-`close` action
(`VetoOnReversalOnNonClose`) and on a `close` with no price window
(`VetoOnReversalWithoutPriceWindow`).

### Tests

- core: default-off skip-serialize, round-trip when set, accepts the
  deprecated `require_price_in_ranges` price window, rejects on non-close,
  rejects without a price window.
- cli: flag rides onto the emitted close when armed + bands present, stays
  off by default, and is suppressed for a news-only reversal-close; the
  paired `05-enter` lists `reversal` in its `vetos` exactly when armed.
- worker: `reversal_veto_plan` scoping (trade_id / account / instrument),
  None without a `trade_id`, and TTL spanning to the window end.

### Follow-up

Experimental — promote past default-off only after a demo run shows it
blocks losers without killing legitimate post-stop re-entries on
multi-shot setups.

## v12 — 2026-06-12 — align remaining workspace crates to broker-tradenation-v0.8.0

### Why

v11 bumped the worker lib's `tradenation-api` pin but missed two other
workspace members that depend on the same git repo: `cli/` (the
`trade-control` CLI) and `tv-arm/`. Both still pinned the old source —
`cli` via `branch = "main"` + `version = "0.1.0"`, `tv-arm` via
`tag = "broker-tradenation-v0.7.0"`. With the lib now resolving the repo to
0.2.0 (`v0.8.0`), `deploy.sh`'s `cargo install --path ./cli` step failed:

```
failed to select a version for the requirement `tradenation-api = "^0.1.0"`
candidate versions found which didn't match: 0.2.0
```

A git dependency unifies to one source per repo across a workspace, so the
mismatched pins also forced Cargo to compile the repo **twice** (v0.7.0 +
v0.8.0 trees side by side).

### What changed

- `cli/Cargo.toml`: `tradenation-api` and `tradenation-instrument-cache`
  moved from `branch = "main"` / `0.1.0` to `tag = "broker-tradenation-v0.8.0"`
  / `0.2.0`.
- `tv-arm/Cargo.toml`: `tradenation-api` moved from `v0.7.0` / `0.1.0` to
  `v0.8.0` / `0.2.0`.
- Neither crate touches the renamed timestamp record fields — `cli` uses the
  client/order/instrument-cache APIs, `tv-arm` only `TradeNationClient` +
  `latest_bid_ask` (in a test). No code changes needed; both compile clean.
- `Cargo.lock` drops the entire duplicate v0.7.0 subtree (−93 lines); the
  workspace now has a single `tradenation-api` source.

### Verification

`cargo install --path ./cli` (the failing deploy step) now succeeds.
Whole-workspace `build --all-targets`, `test` (375 + 112 + 139 + 76 + 23
…), `clippy -D warnings`, `fmt --check`, and the wasm32 lib build all pass.

## v11 — 2026-06-12 — bump tradenation-api to broker-tradenation-v0.8.0

### Why

Upstream `tradenation-api` shipped `broker-tradenation-v0.8.0`
(tradenation-api 0.2.0 / broker-tradenation 0.8.0), which converts all
broker timestamps from London-local to Brisbane (UTC+10) inside the crate
and renames six record fields: the base name now holds the converted
`Option<DateTime<FixedOffset>>` and a new `*_original` sibling keeps the
raw broker string.

### What changed

- Both `broker-tradenation` and `tradenation-api` pins moved from
  `tag = "broker-tradenation-v0.7.0"` to `v0.8.0`.
- Only the **test helpers** in `src/tradenation_adapter.rs` touched the
  renamed fields: `opening_order()`, `position()`, and `closed_trade()`
  now build `period`/`creation_time`/`transaction_date`/`open_period` as
  `None` and set the matching `*_original` to `String::new()`.
- The production matching logic (order-id / ref-id correlation in
  `compute_attempt_state`) reads none of the renamed timestamp fields, so
  it is unchanged. No worker-visible behaviour, wire-format, action, CLI,
  gate, secret, or drawing change — README untouched.

### Breaking

None for this crate's API. The dependency's record structs changed shape
(see upstream), but the worker only constructs them in tests.

### Tests

Existing 112-test suite passes unchanged; wasm32 build verified.


### Why

A `too-high` / `too-low` veto set during one setup could block a later,
unrelated entry on the same instrument. The veto KV key was
`veto:<account>:<instrument>:<name>` — no `trade_id` — and the veto's TTL
is stretched to outlive the setup that set it (`veto_ttl_seconds` extends
to the alert's `not_after` plus a tail). A setup with a multi-day
`not_after` therefore left a live veto key sitting in KV for days, and the
operator's next entry on that pair was silently rejected (HTTP 412
`veto-active`) against a veto they'd forgotten existed. Reported
2026-06-11: a missed trade, the blocking veto set "a long time ago" and
invisible in the recent logs.

### What changed

The veto key now carries the setup id:
`veto:<account>:<trade_id>:<instrument>:<name>`. A veto recorded under one
`trade_id` only blocks entries that carry the **same** `trade_id`; a
veto from a different setup on the same instrument no longer matches. The
`enter` gate looks vetos up by the entry's own `trade_id` (every alert in
a `build-trade` bundle already shares one minted id, so the veto and the
entry it guards agree).

`trade_id` is now **required** on `enter`, `veto`, and `clear-veto` —
`Intent::validate` rejects an intent that omits it
(`IntentValidationError::MissingTradeId`, surfaced as HTTP 400). This is a
hard fail by design (operator decision): every trade needs an id, no
instrument-wide fallback. `MissingTradeId` is checked before the older
`MaxRetriesWithoutTradeId` / `MissingTtlHours` checks, so an untagged
enter/veto now reports the missing id first.

### Breaking

- `StateStore::set_veto` / `is_vetoed` / `clear_veto` gain a `trade_id:
  &str` parameter (after `account`). All impls (KV, in-memory, mocks)
  updated.
- `core::state::clear_named_vetos` gains a `trade_id: &str` parameter.
- `core::state::VetoEntry` gains a `trade_id: String` field (surfaced in
  the `status` snapshot under each `vetos:` entry).
- `cli::build_veto_intent` / `build_clear_veto_intent` gain a `trade_id:
  &str` parameter.

### Config / CLI

- `trade-control veto` and `trade-control clear-veto` gain a required
  `--trade-id <slug>` flag.
- The interactive `sign`/`encrypt` questionnaire now prompts for
  `trade_id` on `veto` / `clear-veto`.

### KV migration

Old `veto:<account>:<instrument>:<name>` keys in the deployed KV are no
longer read (lookups use the new trade_id-bearing key) and TTL out on
their own — no wipe required. Any veto an operator wants gone *now* can be
read back from `trade-control status` (the `vetos:` block lists each
`trade_id`) and cleared with `clear-veto --trade-id`.

### Tests

- core: `validate_rejects_enter_without_trade_id`,
  `validate_rejects_veto_without_trade_id`,
  `validate_rejects_clear_veto_without_trade_id`,
  `validate_accepts_veto_with_trade_id`;
  `memstore_veto_scoped_per_trade_id` (veto under trade A does not block
  trade B on the same instrument + account). Existing enter/veto validate
  tests updated to carry a `trade_id`.
- cli: `veto_intent_round_trips` / `clear_veto_intent_carries_name` now
  assert the `trade_id` is set and the built intent validates.

All green: core 375, worker 112, cli 230 + 8; clippy + fmt clean on host
+ wasm.

## v9 — 2026-06-10 — calendar-bars resolves instruments via instrument-lookup

### Why

`calendar_bars::parse_instrument` resolved the trade's instrument through
the legacy `trade_calendar_maker::Instrument::from_oanda_symbol`, which
only understands OANDA forex-style symbols (`EURUSD` after stripping
`_`/`/`). TradeNation index and spread/diff MarketNames — e.g.
`Wall St 30 / Germany 40 Rolling Future Diff` (chart symbol `US30DE40`) —
failed with `unsupported instrument symbol`, so the `calendar-bars` step
was silently skipped (caught as a WARN) during a `tv-arm` run, producing
no auto pause/news bars for that setup.

### What changed

`parse_instrument(raw, broker)` now resolves through the canonical
`instrument-lookup` catalog: by the broker's own symbol first (the form
the caller passes; `broker` is carried on `CalendarBarsArgs`), then a
broker-agnostic `resolve` for canonical ids / cross-broker symbols. The
`Instrument` is built from `asset.news_currencies` (→ `affected_currencies`,
the only field consumed downstream via `is_affected_by`) and `asset.class`
(→ `InstrumentType`; `Crypto`/`Stock` fold into `Index`). Retires one of
the partial instrument maps flagged for migration in `CLAUDE.md`.

### Breaking

- `cli::parse_instrument` gains a second argument:
  `parse_instrument(raw: &str, broker: BrokerKind)`. There is **no** legacy
  `from_oanda_symbol` fallback — an instrument the catalog doesn't know is
  now a hard error pointing the operator at
  `~/.config/instrument-lookup/mappings.toml`, instead of silently
  mis-deriving news currencies from a string heuristic.

### Config

- New `instrument-lookup` path dependency on the `cli` crate. Instruments
  not in the baked-in catalog (e.g. TradeNation diff/spread CFDs) need an
  `[[asset]]` overlay entry in `~/.config/instrument-lookup/mappings.toml`.

### Tests

- `parse_instrument` tests rewritten for the new signature and catalog
  backing: OANDA `EUR_USD`, TradeNation `CHF/JPY`, a multi-word TradeNation
  index name (`Germany 40`) the legacy parser couldn't handle, a canonical
  id (`US30`), and rejects-unknown. 230 cli lib tests pass.

### Verified

- `trade-control calendar-bars --instrument
  "Wall St 30 / Germany 40 Rolling Future Diff" --broker tradenation` — the
  name that previously threw — now resolves, keeps the USD CPI event, and
  writes pause+news bundles.

### Note

- Cargo `version` bumped `0.1.0 → 0.2.0` (root `trade-control-web-hook` and
  `cli/trade-control-cli`).

## v8 — 2026-06-09 — bind Pine alertconditions by title, not positional `plot_N`

### Why

A live `tv-arm` run failed `05-enter` and `06-close-on-reversal` with
`err.code="general"` — the catch-all TradingView returns when an
alertcondition's `plot_N` index doesn't resolve. Root cause: the
`PLOT_LONG_PATTERN`/`PLOT_SHORT_PATTERN`/`PLOT_EVERY_BAR_CLOSE` constants
were positional plot indices, and v2.3's five `next_candle_timestamp_1..5`
plots (added between `recent_low` at plot_9 and the alertconditions) had
silently shifted the three alertconditions from `plot_10/11/12` to
`plot_15/16/17`. The constants were never updated, so the alert payloads
pointed at numeric series instead of alertconditions. The error code is
identical to a stale-compile-cache, so it masqueraded as the
"republish the script" case (which it survived).

### What changed

- **Immediate fix:** corrected the three plot constants to `plot_15/16/17`.
- **Structural fix (the real one):** alertconditions are now bound by their
  **title** (`"Long Pattern"`, `"Short Pattern"`, `"Every Bar Close"`)
  rather than a positional `plot_N`. The `tv-arm` JS template resolves the
  title → live `plot_N` at create time from the study's `metaInfo()`
  (`metaInfo().plots` filtered to `type === "alertcondition"`,
  cross-referenced with `metaInfo().styles[id].title`). Adding or removing
  `plot()` calls in the Pine source can no longer break the binding.
- A title absent from the published study fails that alert **loudly**,
  listing the alertcondition titles it did find — no positional fallback
  (a guessed index is exactly the silent failure this removes).
- Verified against the live chart: the resolver maps the three titles to
  `plot_15/16/17`.

### Breaking

- `conventions`: `PLOT_LONG_PATTERN`/`PLOT_SHORT_PATTERN`/
  `PLOT_EVERY_BAR_CLOSE` and `entry_plot_for`/`reversal_close_plot_for` are
  removed, replaced by `ALERT_LONG_PATTERN`/`ALERT_SHORT_PATTERN`/
  `ALERT_EVERY_BAR_CLOSE` (title strings) and `entry_alert_for`/
  `reversal_close_alert_for`.
- `tv-arm`: `AlertPayload::PineAlertcondition`'s `alert_cond_id` field is
  renamed `alert_cond_title`.

### Config

- None. Operators must keep the alertcondition **titles** in
  `conventions/src/pine.rs` in lockstep with the `alertcondition()` calls
  in `pine-scripts/candle-signals-v2.pine` — but no longer track plot
  indices.

### Tests

- conventions 33, tv-arm 139 — green. Renamed the plot-id asserts to
  title asserts; no positional `plot_N` left in Rust.

### Follow-up

- None outstanding; the plot-index-drift failure class is closed.

## v7 — 2026-06-09 — `--version` reports the git tag/commit

### Why

The CLIs had no useful way to report which build was running. `tv-arm`
exposed clap's `--version` but it printed the never-bumped crate version
(`0.1.0`); `trade-control` had no `--version` at all. After a deploy/build
you want to confirm you're on the version you think you are.

### What changed

- Both `trade-control` and `tv-arm` now report the git tag/commit on
  `--version`, captured at build time via a `build.rs` running
  `git describe --tags --dirty --always` (e.g. `tv-arm v7`,
  `trade-control v7-2-gabc123-dirty`). Falls back to the crate version when
  git isn't available (source-tarball builds).

### Config / Breaking

- None. Adds a `build.rs` to the `cli` and `tv-arm` crates and a
  `GIT_VERSION` compile-time env var.

### Tests

- cli 227, tv-arm 139 — green; `--version` verified to print the describe
  string for both binaries.

## v6 — 2026-06-09 — bake `pip_size` into the signed enter intent

### Why

The worker scales every `offset_pips` into a price with
`price = anchor + offset_pips * pip_size` and binds `pip_size` into the
gate-script scope. For H&S enters that pip came from `pip_size_for`: a
`PIP_SIZE_<instrument>` secret falling back to a forex-shaped `0.0001`
default — silently 100× wrong for JPY pairs and 10000× wrong for indices
unless an operator remembered to set the secret. The worker is WASM and
links no instrument catalog, so it never read the (now-correct)
`instrument-lookup` pip. M/W already solved this by baking pip into the
signed `MwParams`; this extends the same approach to H&S and any non-M/W
enter.

### What changed

- Pip is now baked at arm time and read from the signed intent. Worker
  precedence (`run_enter`): baked `intent.pip_size` → `PIP_SIZE_<instrument>`
  secret → `0.0001` default. The fallback keeps pre-baked in-flight intents
  resolving during rollout.
- `tv-arm` resolves `asset.pip_size` from `instrument-lookup` for the H&S
  path too (previously M/W-only) and bakes it; `--pip-size` override now
  applies to both H&S and M/W.
- `pip_size` is already a gate-script variable (`allow_entry`, `min_r`, …);
  the bound value is now the baked pip.
- No worker-side catalog lookup and no live spread fetch on the hot path —
  pip arrives baked in the signed message.

### Config

- New optional signed field `pip_size` on the enter intent (top-level).
  Absent = the worker falls back to the secret/default (pre-feature
  behaviour); the wire form stays byte-identical when absent.
- `PIP_SIZE_<INSTRUMENT>` secret is now an override/fallback, no longer the
  primary source. Arming through `tv-arm` no longer needs per-instrument
  secrets for JPY pairs or indices.
- New CLI/`TradeSpec` field `pip_size: Option<f64>`.

### Breaking

- None on the wire (additive optional field). `IntentValidationError` gains
  a `PipSizeInvalid` variant; `build_enter_alert` (cli, internal) gains a
  `pip_size` parameter.

### Tests

- core: validate accept/reject (zero/negative/NaN), serde elision +
  round-trip, signed wire round-trip + tamper-rejection, script-visibility
  of `pip_size`.
- cli: H&S enter carries baked pip; omitted when spec has none; M/W enter
  carries matching top-level + `mw.pip_size`.
- tv-arm: H&S spec bakes catalog pip; `--pip-size` overrides on H&S.
- Totals: core 371, cli 233, tv-arm 139, worker 112 — all green; WASM root
  builds.

### Follow-up

- Once all live intents are armed through the updated `tv-arm`, the
  `PIP_SIZE_<instrument>` secrets can be dropped.

## v5 — 2026-06-08 — bar-based pending-order expiry (`expiry_bars`)

### Why

A resting stop-entry whose breakout never happens otherwise sits until
`not_after` (the whole alert window). For a breakout setup, the clean edge
is gone within a few bars — we want to cancel a never-filled order N bars
after placement. Neither broker has a native per-order expiry (TradeNation
orders are hardcoded Good-Till-Cancel; the OANDA worker path uses GTC
too), so the worker must enforce it.

The hard part: "N bars from now" must skip weekends / session breaks, and
a resting order gets **no further webhooks** to count bars from — so the
worker can neither count fires nor (lacking a session calendar) convert
bars→wall-clock across a Friday→Monday gap. Only the indicator can: Pine's
`time_close(timeframe.period, bars_back=-N)` projects forward respecting
the symbol's session schedule.

### What changed

**Wire format (new field + menu)**

- New signed `Intent::expiry_bars: Option<Tunable<u32>>` (1..=5) on the
  enter intent — the author's policy, chosen at arm time.
- New unsigned shell menu `next_candle_timestamp_1..5` (in
  `UNSIGNED_VALUE_KEYS`, routed onto `Shell` in `incoming`) — Pine fills
  the absolute forward bar-close timestamps at fire time. New
  `Shell::next_candle_timestamp(n)` accessor.

**Worker**

- New `core::intent::resolve_cancel_at(expiry_bars, shell, not_after)`:
  picks `menu[expiry_bars]`, falls back to `not_after` on a missing slot,
  caps at `not_after`, and returns `ExpiryError::OutOfRange` for 0 / >5.
- `run_enter` resolves `expiry_bars` (Phase-1 scope, like `max_retries`)
  and computes `cancel_at` **before** any broker work; an out-of-range
  value → `Rejected` 400 `expiry-bars-out-of-range` (does **not** mark the
  id seen — next bar can retry).
- New `EntryAttempt::cancel_at` (additive, `#[serde(default)]`), threaded
  through `retry_gate::record_placement`. Deliberately **separate** from
  `expires_at`, which stays tied to `not_after + grace` (it drives the KV
  row TTL and replay/retry-gate record lifetime — shortening it would age
  records out early).
- Cron sweep: new OR-branch cancels a pending order once `cancel_at` has
  passed, logged `reason=bar-expiry` (distinct from `expired`). Pure
  `bar_expiry_due` predicate added.

**CLI / tv-arm / Pine**

- `TradeSpec::expiry_bars` → threaded onto the `05-enter` intent only.
  `wrap_signed_template` appends the menu placeholders **only when
  `expiry_bars` is set**, so non-expiry trades stay byte-identical and
  don't depend on the new plots.
- `tv-arm --expiry-bars N`.
- `candle-signals-v2.pine` v2.3: five `next_candle_timestamp_1..5` hidden
  plots via `time_close(timeframe.period, bars_back=-k)`.

### Breaking

None. `expiry_bars` absent = today's behaviour (rest until `not_after`);
old KV `EntryAttempt` rows without `cancel_at` decode as `None`.

### Config

- `expiry_bars: <1..5>` on an enter intent / trade spec; `--expiry-bars`
  on `tv-arm`. Requires the v2.3 indicator that ships the menu plots.

### Tests

- core: sig keeps the menu unsigned; incoming routes the menu onto Shell;
  `expiry_bars` round-trips on Intent; `resolve_cancel_at` slot pick /
  out-of-range / missing-slot fallback / not_after cap; `EntryAttempt`
  JSON round-trips with and without `cancel_at` (incl. legacy-row default).
- worker: `bar_expiry_due` predicate; `expiry-bars-out-of-range` outcome
  classifies as Skip (no id poison).
- cli: `expiry_bars` threads onto enter only; menu present/absent in the
  signed body by opt-in; end-to-end sign→substitute→verify round-trip.

### Follow-up

- `on_broker_rejection` recovery (skip/market/limit on `#19-10`, with a
  ≥1R recheck and limit-override) — deferred; brief in
  `BUG-entry-too-close-to-market.md`.
- Pine `time_close` forward projection can't anticipate an *unscheduled*
  one-off holiday inside the window; `not_after` is the backstop.

## v4 — 2026-06-08 — `prep-expire`: a `<prep>-expiry` cutoff line

### Why

An H&S setup is only valid if the break-and-close lands within a bounded
number of bars of the pattern start (M15/H1 30–120, H4 30–180, Daily
30–210, Weekly 30–∞). A real demo trade lost because the break-and-close
came **124 bars** after the pattern start on H1 (max 120) — the pattern
had grown too big to be a clean H&S, but nothing on the chart stopped the
entry. Operators needed a way to draw that cutoff.

### What changed

**`prep-expire` action (new)**

- New `Action::PrepExpire` (wire `prep-expire`). Carries `step` (which
  prep) + `trade_id` + `ttl_hours`. State-only, no broker call.
- New `StateStore` methods `block_prep` / `is_prep_blocked` /
  `clear_prep_block` over a dedicated `prep-blocked:<scope>:<instrument>:<step>`
  keyspace (global-first lookup, account-scoped, TTL-gated — same shape as
  vetos but its own namespace). New `PrepBlockEntry` +
  `Snapshot.prep_blocks` so blocks show in `status`. `PREP_BLOCK_INDEX_CAP`.
- Worker: `handle_prep_expire` stores the block and logs `prep-expire
  stored`; `handle_prep` now rejects a blocked step with a 409
  `prep-expired` and a `prep rejected — expired` log. The rejection is
  `Rejected` (does **not** poison the seen-id, per the 2026-06 replay-scope
  rule), so a re-fire just re-logs. The enter gate's existing
  `missing-prep` log completes the three-line timeline a future debugger
  can grep to reconstruct the trade.
- A prep that already fired *before* the block is untouched — the block
  only stops *future* preps, so a trade that legitimately entered is not
  disturbed.

**Chart side (`<prep>-expiry` line)**

- New drawing label vocabulary: a vertical line `<prep>-expiry`
  (`break-and-close-expiry`, `retest-expiry`, plus `neckline-expiry` /
  `retrace-expiry` aliases). `trade-expiry` keeps its dedicated
  whole-trade-close meaning — a prep named `trade` would collide, but
  that's illogical. `conventions::prep_name_from_expiry_label` resolves
  the canonical prep step.
- New `AlertBasename::PrepExpire(step)` → `08-prep-expire-<step>`.
- CLI `TradeSpec.prep_expiries: Vec<String>` emits one drawing-bound
  `08-prep-expire-<step>` alert per cutoff line. Rejected if a name isn't a
  known prep or is also in `skip_preps`.
- `tv-arm` classifies `<prep>-expiry` lines into `Roles.prep_expiries`,
  binds each to its drawing, and **validates**: a future cutoff with no
  matching prep trend line is a hard error (the setup could never enter);
  a past cutoff is a warning (re-arming later in time).

### Wire / config

- `Intent` gains `action: prep-expire`; `validate` requires `step` +
  `ttl_hours` (`MissingPrepExpireStep`).
- `TradeSpec` gains `prep_expiries` (omitted from serialised yaml when
  empty — byte-identical for existing trades).

### Tests

conventions label-resolution + basename round-trip; core validate (well
-formed / no-step / no-ttl) + block round-trip + account scoping + snapshot
yaml; CLI emitter + reject-unknown + reject-skipped; tv-arm classify +
latest-wins + future-error / past-warn / future-with-prep-ok + alert
binding. Host + wasm build, clippy + fmt clean across all five crates.

### Follow-up

The cutoff timestamp is operator-drawn; nothing yet auto-computes the
bar-count limit per timeframe. A future pass could draw the `<prep>-expiry`
line automatically at `pattern_start + max_bars × resolution`.

## v3 — 2026-05-28 — News-event blackout pauses + drawing-alert hardening

### Why

Macro news events (NFP, CPI, central-bank decisions) cause spike risk that
makes pending H&S setups dangerous to enter during the window. Before this
release, the only way to suppress a single trade across a news event was to
manually veto and remember to clear it. This release adds a first-class
`pause` / `resume` action pair keyed by `(trade_id, blackout_id)` so a
trade can carry multiple concurrent blackout windows independently.

Alongside it, several drawing-alert + signing fixes that had been
accumulating on the working tree: vetos and preps now use a drawing-only
shell (no `{{plot("…")}}` placeholders, which were crashing the worker's
YAML parser when delivered literally), and the signing path covers
`recent_high` / `recent_low` from Pine v2's 2026-05-26 update.

### What changed

**Pause / resume action (new)**

- New `Action::Pause` and `Action::Resume` variants on the `Intent`
  enum, with two new optional fields: `blackout_id` (slug, required on
  pause/resume) and `reason` (free-form label).
- New KV key shape `pause:<trade_id>:<blackout_id>` — pauses are
  per-trade, not per-(account, instrument), so multiple concurrent
  windows on a trade (NFP + central-bank, etc.) coexist as siblings.
- New `StateStore` trait methods: `set_pause` / `list_pauses_for_trade`
  / `clear_pause`. Implemented on both `MemStateStore` (tests) and
  `KvStateStore` (production); listing uses `kv.list` prefix scans.
- Worker dispatch: `Pause` / `Resume` handled in Stage 1, no broker
  call. `run_enter` gains a top-of-pipeline blackout gate that rejects
  with 423 and outcome `paused: [<blackout_id>(<reason>), ...]`
  whenever any pause for the trade is active. Sits ahead of the retry
  gate so a paused trade doesn't burn retry slots.
- New CLI: `trade-control build-pause --from-file <pause.yaml>
  --key-file <key> --output-dir <dir>` emits a signed `01-pause-<id>` /
  `02-resume-<id>` pair plus a `manifest.yaml`. Pure drawing-shell
  alerts — they fire from `LineToolVertLine` time-crosses, not Pine.
- `Snapshot` (the `status` action's response) now includes a `pauses:`
  section listing every active blackout across every trade. Back-compat
  for older serialised snapshots is preserved via serde defaults.

**Python: `tv_arm_hs.py` blackout detection**

- New `BLACKOUT_START_LABELS = {"blackout-start", "pause"}` and
  `BLACKOUT_END_LABELS = {"blackout-end", "resume"}` — interchangeable
  aliases.
- `classify()` collects every matching vertical line into
  `roles.blackout_pairs`. `pair_blackouts()` sorts them chronologically
  and pairs positionally; **odd counts and reversed pairs are hard
  errors that abort the whole run** (including the H&S bundle) — a
  misdrawn chart shouldn't be allowed to arm half a blackout window.
- Per blackout pair, the script writes a `pause.yaml` and shells out to
  `trade-control build-pause`, then maps the resulting `01-pause-*` /
  `02-resume-*` basenames to vertical-line time-cross alerts and stacks
  them onto the H&S `payloads` list for `create_alerts`.

**Drawing-alert + signing fixes (bundled WIP)**

- `wrap_signed_template_drawing` (new) emits a drawing-only shell with
  just `close`/`high`/`low`/`time` placeholders. `wrap_signed_template`
  (renamed concept) keeps the full Pine-bound shell. `trade_patterns`
  picks per-alert: only `05-enter` is Pine-bound; vetos and preps use
  the drawing shell. Fixes 19 rejections/day from `{{plot(...)}}`
  arriving literally and crashing the YAML parser.
- `core::sig::UNSIGNED_VALUE_KEYS` now includes `recent_high` /
  `recent_low` — Pine v2 from 2026-05-26 emits these via
  `{{plot(...)}}`, and the worker treats them as optional shell fields
  for `recent_high` / `recent_low` SL anchoring.
- `IncomingError::BadYaml` and `BadIntentYaml` now carry the underlying
  serde error message so the worker log explains *why* a body was
  rejected. Rejected bodies are also logged in truncated excerpt form
  (cleartext YAML already passes through CF's request log, so no new
  exposure).
- `tv_arm_hs.py` TradeNation instrument resolution now falls back to
  the chart's description ("Germany 40", "Spot Silver") when the raw
  symbol misses the catalog — TN's catalog has FX/stocks but not most
  indices/commodities.
- `build-trade --from-file` now rejects spec accounts that aren't in
  the local CLI history cache, catching typos before they reach the
  worker.

### Breaking

- `Intent` gains two new fields (`blackout_id`, `reason`); both are
  `Option<String>` with `skip_serializing_if`, so the wire form stays
  byte-identical for pre-existing intents. In-tree struct-literal
  callers (8 sites) updated.
- `StateStore` trait gains three new required methods — any future
  out-of-tree implementor will need to add them. All in-tree
  implementors (KV, mem, retry-gate test stub) updated.
- `Snapshot` gains a `pauses: Vec<PauseEntry>` field with
  `#[serde(default)]`; older serialised snapshots still parse.

### Config

- No new env vars or secrets.
- `pause.yaml` spec schema for `build-pause --from-file`:
  ```yaml
  trade_id: eurusd-hs-1            # required, matches parent enter alert
  blackout_id: nfp-2026-06-06      # optional, auto-minted from epoch
  start_time: "2026-06-06T12:30:00Z"
  end_time:   "2026-06-06T13:00:00Z"
  reason: "news:USD-NFP"           # optional, surfaces in seen-index
  instrument: EUR_USD
  account: oanda-reversals-demo
  broker: oanda                    # default
  ```

### Tests

- `core`: 4 new intent validation tests (pause requires trade_id +
  blackout_id, bad blackout_id rejected, well-shaped pair accepted,
  YAML round-trip). 3 new memstore pause tests (round-trip, multiple
  blackouts per trade, isolated per trade_id). Snapshot serialisation
  test extended.
- `cli`: 8 new `pause_pattern` tests including an end-to-end
  build → sign → `parse_and_verify` round-trip with simulated TV
  shell substitution.
- 1 new test confirming drawing alerts emit no Pine plot placeholders.
- All 523 unit tests (306 core + 166 cli + 51 worker) green. Clippy
  clean across all three crates with `--all-targets`. Python script
  syntax-checked.

### Follow-up

- ForexFactory MCP integration: Claude still draws blackout lines
  manually via tv-mcp; a future `tv_draw_blackouts.py` helper could
  automate from FF event data.
- The pause-bundle output directories under `<arm-out>/<sym-date>/pause-N/`
  pile up over time — a janitor pass to prune dirs older than N days
  would help.
- Optional `kv.list(prefix="pause:")` janitor in the worker to expire
  orphaned pauses past N days (today they ride on the alert's
  `not_after + grace` TTL, which is usually enough).
