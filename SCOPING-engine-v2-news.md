# SCOPING — engine-v2 economic-news systems

Status: **pause slice DONE; news-reversal-close slice PENDING (design below).**

## Two separate systems (from the v1 map)

The live v1 worker handles economic news with **two distinct mechanisms** —
different KV namespaces, different `Action`s, different effects. Neither is the
market-hours "blackout" (spread-hours, `core/src/intent/blackout.rs`) nor
sentiment (`news-sentiment-tv`, human arm/journal time only — no runtime effect).

| | **Pause** (System 1) | **News-reversal-close** (System 2) |
|---|---|---|
| Window | `[event − before, event]` (8h H1+, 3h M15) | `[event, event + 1h]` |
| Effect | **blocks entries** during the standoff | **closes an open trade** on a counter-trend reversal candle |
| v1 state | `pause:<trade_id>:<blackout_id>` KV | `news:<trade_id>:<news_id>` KV → `PlanState.open_news_windows` |
| v1 arm | `Pause`@start + `Resume`@event (TimeReached control) | `NewsStart`@event + `NewsEnd`@event+1h |
| v1 enforce | enter gate: any pause active → 423, no order | `Close` intent, `PinePattern{opposite-dir}` reversal AND candle-quality/`allow_close`, contextually gated by `inside_window` (news-open OR price-band, OR-composed) → `broker.close_positions` |
| Terminal? | no (auto-resumes at event) | **non-terminal** (flattens, plan stays for multi-shot) |

## Data source — already exists, reused as-is

The calendar + windowing is **already built** in `trade-calendar-maker`
(`~/projects/trade-calendar-maker`) over `forex-factory`:

- `Timeframe` bucket → impact/buffer (`types/timeframe.rs`): **M15 = 2★+, 3h before / 1h after; H1Plus = 3★ only, 8h before / 1h after**; `<15m` = no news bars.
- **USD 3★ (High) blocks EVERYTHING** (`green_windows.rs::affected_instruments_for_event`), not just USD pairs.
- `tv-arm` resolves windows via `cli/calendar_bars.rs::plan_calendar_bars_within` and bakes `NewsWindow{start,end}` (`tv-arm/src/news_window.rs`) onto the plan; `--news-before-hours` / `--news-after-hours` override the buffers per run (operator is experimenting with 3h vs 8h standoffs).

engine-v2 does NOT re-derive any of this — a future `tv-arm-v2` calls
`trade-calendar-maker` and bakes the windows onto the v2 plan, exactly as tv-arm
does today.

## Pause slice — DONE (`177fd68`→`fab0089`)

Blocks entries during the `[event − before, event]` standoff; auto-resumes at
the event. Fully fact-based, reuses the 4c/4d guard pattern — no new broker
effect, no detector. 4 reviewed/green steps:

1. **`NewsWindow{start,end}`** in types-v2 (`window.rs`) — the plan-data version
   of tv-arm's window: `contains(t)` start-inclusive/**end-exclusive** (so the
   pause resumes at `end`), `is_past(t)`, `new()` normalises. The window is the
   unit (can't split — avoids the old drawn start/end-line orphaned-half bug).
   `TradePlan.pause_windows: Vec<NewsWindow>` (`#[serde(default)]`).
2. **`Paused` fact kind** — a plan-scoped `Flag(bool)`, **NOT latching** (unlike
   `Invalidated`): `Flag(true)` inside a window, `Flag(false)` outside.
   `Facts::flag`/`flag_named` distinguish `Some(false)` from unset (`None`).
3. **`Pause` rule** + `RuleKind::Pause` — recomputes membership each tick against
   **wall-clock `now`** (matching v1's PR2 control-window gating — the OPPOSITE
   clock from `Expiry`, which uses `candle.time`/spine semantics), edge-triggered
   (writes only on a change), NOT fire-once (re-pauses for later events). No
   geometry type param (reads plan-level `pause_windows`, like `Enter`).
4. **Enter third guard** — after the two retire guards, `if paused flag == true
   { return }`. **Temporary, not terminal**: the flag clears at the window end and
   the enter is live again next tick (auto-resume). End-to-end test:
   `enter_blocked_during_pause_then_places_after`.

## News-reversal-close slice — PENDING (design)

Closes an open trade on a counter-trend reversal candle during
`[event, event + 1h]`. This is the **bigger** slice: it introduces engine-v2's
**first position-closing broker effect** and needs the reversal detector.

Sketch (subject to a build-time review like the earlier slices):

- **Model**: `TradePlan.news_windows: Vec<NewsWindow>` (the `[event, event+1h]`
  spans — separate from `pause_windows`).
- **Fact**: a plan-scoped `in_news` flag, maintained by a rule exactly like
  `Pause` (toggles on window membership) — OR reuse the same rule shape over
  `news_windows`. Mirrors v1's `open_news_windows`.
- **The reversal-close rule**: reads (a) the `in_news` flag (OR a price-in-band
  condition — v1 OR-composes news-window and S/R-band, and the OR is
  load-bearing: an earlier AND dropped S/R-only reversals, EUR_USD 2026-07-08),
  (b) runs the **opposite-direction reversal-candle detector** (v1's
  `eval_pine_entry` with the trade's opposite `dir` — the same stateful detector
  the entry uses), (c) AND-gated with candle-quality (`needs_golden` /
  `needs_confirmed`) and the `allow_close` script, (d) spread-hour "rubbish
  candle" suppression (don't flatten off a liquidity-vacuum wick).
- **New `Effect::ClosePosition`** — the first acquisitive-but-not-place effect.
  The driver executes it against the async `Broker` (`close_positions`). Like
  `PlaceOrder` it's latest-bar-gated (don't chase a stale close). NON-terminal:
  the plan stays live for multi-shot re-entry (v1: a reversal-close guard flattens
  but keeps `AwaitEntry`).

### Vocabulary — CLOSE vs VETO/INVALIDATE (per the CLAUDE.md glossary, v97)

The reversal-close is a per-**POSITION CLOSE** — *not* an "exit" (a false start;
settled by v97 `052a9fb`, `RuleKind::PerTradeExit` → `PerPositionClose`) and *not*
a veto. Two axes disambiguate the whole family (does it touch the open position ×
does it stop future entries):

| glossary concept | open position | future entries | v2 rule/effect |
|---|---|---|---|
| **CLOSE** (reversal-close) | **flatten one** | **left open** | *pending* `PerPositionClose` → `Effect::ClosePosition` |
| **VETO / INVALIDATE** (too-high/too-low, 80%-to-TP pcl-exhausted) | **leave alone** | **block all** | `InvalidateHigh`/`InvalidateLow` → `Effect::Invalidate` (**built**, already StopNextEntry-only) |
| **CLOSE-VETO** (rare, true thesis-death) | **flatten** | **block** | not built |

So the v2 reversal-close **flattens one position and does nothing else** — it does
NOT block future entries; the trade lives and may re-enter. Blocking future
entries is the *separate* job of the invalidation caps (already built as v2
`Invalidate`), which conversely leave an open position running to its own SL/TP.
My v2 `Invalidate` already documents itself as StopNextEntry-only / never-closes —
so it matches the glossary's VETO/INVALIDATE row by construction.

- **Writes NO veto (v96, `a9e9b2c`, 2026-07-19).** A gate-passed reversal-close
  writes no `reversal` veto. **Do NOT port a `reversal`-veto write** —
  `veto_on_reversal` is a dormant no-op in v1 (field/flag still parse & round-trip
  for back-compat; full removal deferred).
  - *Why removed:* it was the last confirmed **replay↔live divergence** — the live
    worker wrote the veto in `run_close`, which the offline replay never calls, so
    a later multi-shot enter passed offline but rejected live. In v2 this class of
    bug **can't exist by construction**: replay and live run the identical pure
    rule, differing only in the Broker/Storage impls. Reinforces the v2 design.

Watch-points for that slice:
- The **reversal detector** is the real work — engine-v2 has no Pine detector yet.
  It exists twice in v1 (Pine + `core/src/signals/`); reuse the `core` copy.
- **OR not AND** for the news/price close gates (the load-bearing fix above).
- **Close-only** (see the vocabulary table) — flattens one position, writes no
  veto; blocking future entries is the invalidation caps' job. Name it
  `PerPositionClose`, not "exit".
- The close effect needs the **async Broker** threaded into the driver's execute
  step — the same boundary the `PlaceOrder` executor slice will introduce. May
  be worth doing the executor slice (place-order execution) first so the close
  effect lands on an established broker-execute path.

## Keep this in sync — the v1 news code is moving

The live v1 news/close code gets bug-fixed as the demo runs; a design mapped one
week can be stale the next. Before building the reversal-close slice, **re-check
`git log --since` on** `core/src/dispatch/close.rs`, `core/src/pause_gate.rs`,
`engine/src/evaluate.rs`, `core/src/signals/`, `core/src/intent.rs`,
`conventions/src/roles.rs`. Known moves so far: v96 close-writes-no-veto + v97
terminology (`PerTradeExit` → `PerPositionClose`, "close" not "exit"; see the
CLAUDE.md CLOSE vs VETO/INVALIDATE glossary); the OnClose-origin-side /
cross-buffer-atr / slope-scaled-retest engine tuning (System-agnostic).
`pause_gate.rs` (System 1) has NOT changed since the pause slice shipped, so the
built pause behaviour is
still a faithful port.
