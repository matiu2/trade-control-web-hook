# TODO — `journal` TUI (trade-journalling operator tool)

## STATUS 2026-07-23 — v1 SHIPPED, verified live on staging
Commits `042fdd7` (scaffold), `3912950` (TUI), `f6612b8` (fired-rule fix).
Installed as `journal-staging` / `journal-dev`. All screens proven end-to-end
in a real terminal (tmux) against the live staging worker: List → Timeline
(info bar: `AUD/CAD · h1 · short │ normal (break+close+retest) (BCR stop) │
outcome`) → Replay (full `replay-candles` report) → Compare (replay ‖ live
side-by-side). Delete guard blocks unopened plans; confirm modal, `i` detail
popup, and `←`-unwind all work. 13 tests (incl. 2 TestBackend render tests).

**Done (v3):**
- **Arm-time screenshot** — SHIPPED. `tv-arm register` reads the system
  clipboard (`wl-paste` → `xclip` → `xsel`) and, when it holds a TradingView
  snapshot URL (`https://www.tradingview.com/x/<id>/`), bakes it onto the plan
  as `TradePlan.screenshot_url`. Workflow: hit TV's camera button, then arm —
  the chart as drawn is pinned to the trade. Fail-soft like `armed_sentiment`:
  any other clipboard contents (or no clipboard tool) yields `None` and arming
  proceeds silently. Recognition lives in `core/src/screenshot.rs`
  (`ScreenshotUrl`; the only constructor is `parse`) so tv-arm and the journal
  share one answer, and is deliberately narrow — a TV *chart* link or any other
  URL is **not** a screenshot.
  The journal shows it on a second info-bar line (the bar grows 3→4 rows only
  when a URL is present) and `o` opens it via `xdg-open`. The URL is
  **re-validated** through `ScreenshotUrl::parse` when read back from the plan,
  so a stored plan can't hand an arbitrary URL to the browser. Deliberately
  *not* an OSC 8 terminal hyperlink: ratatui measures span width with
  unicode-width over the raw string, so the escape bytes would be counted as
  visible cells and smeared one-per-cell through the buffer (ratatui#902) —
  `o` is the reliable mechanism.

**Done (v2):**
- **Compare diff** — SHIPPED. `journal/src/divergence.rs` extracts fire facts
  keyed by `rule_id` from both sides (live = `ticks[].eval.fired[]`; replay =
  the `<ts>  <LABEL> (<rule_id>) — …` report lines), normalises both timestamps
  to `YYYY-MM-DD HH:MM` Brisbane, and `diff()` classifies match / live-only /
  replay-only / timing. The Compare screen leads with a coloured divergence
  summary band + a detail list over the raw side-by-side. Also parses the
  replay summary line (`Done` / final phase / fires / TP / SL / Net R) for a
  coarse outcome sanity-check. Verified live on staging against AUD_CAD:
  **4 matched rule ids, 4 timing divergences** (live fires pause/resume/news
  spread across 03:30–12:30, replay fires all four at 13:00). 9 divergence unit
  tests + 1 Compare TestBackend render test.

**Done (v2):**
- **Async loading** — SHIPPED. `journal/src/jobs.rs`: slow shell-outs (replay,
  timeline+export, TV annotate) run on a `std::thread` and post a `JobResult`
  over an mpsc channel; the event loop drains it each tick. `App` tracks an
  `in_flight` set (never double-spawns, drives the spinner) and a `tick` counter
  animates a braille spinner in the footer + Replay screen. The UI stays fully
  responsive during a ~25s replay — verified live: navigated screens mid-replay,
  the spinner kept animating, and the report landed on completion. 3 job unit
  tests (`drain_applies_timeline…`, `drain_surfaces_failure…`, noop). Delete
  stays synchronous (fast + deliberately blocking).

**Done (v2):**
- **TV load** — SHIPPED. `journal/src/tv.rs` drives tv-mcp to *navigate* the
  live chart (symbol via instrument-lookup → timeframe → scroll-to-armed_at →
  zoom out ~3×). Navigation only, no drawing. Auto-fires on the first `→`
  (Timeline screen) and re-fireable with `l`; runs as a background job (footer:
  "loading TradingView…" spinner). Commands shell `node <tv-mcp>/src/cli/…`
  with 1s sleeps between (calibrated interactively — TV needs a beat). Times are
  passed as **unix timestamps** (not date strings — the Node side parses bare
  dates in local TZ). Verified live: AUD/CHF plan → symbol OANDA:AUDCHF, tf 60,
  scrolled + zoomed. 4 tv unit tests.
  - **Known caveat**: near the live data edge the ±75-bar zoom window is
    clamped/loosened by TV (a plan armed a few days ago with data through *now*
    shows a wider span). Historical/archived plans (the journalling norm) centre
    cleanly. Not worth tightening unless it annoys.
  - **Already-there short-circuit (2026-08-04)**: `load_chart` now READS the
    chart first (`tv state`) and returns early if the symbol + resolution already
    match, so `l` / replay / `s` are idempotent. Measured live: an already-there
    hit is **~75-150ms vs ~13s** for a real load (node spawns + TV settling
    dominate, not the 1s sleep). The bigger win is that setting the symbol
    **resets the operator's scroll position** — re-loading a correct chart threw
    away exactly what they'd navigated to. `tv state` returns the symbol
    fully-qualified (`TRADENATION:EURCAD`) and the resolution in TV form (`240`),
    i.e. the same forms `tv_symbol`/`tv_resolution` emit, so it's plain string
    equality with no second mapping to drift. **Any doubt → load**: `success:
    false`, a missing field, unparseable JSON or a spawn failure all fall through,
    because a needless load is slow-but-recoverable while a wrong skip strands
    tv-arm on the wrong chart (a wrong answer). `JobOutcome::LoadTv` carries
    `already_there` so the footer says which happened. 9 tv tests (13 total),
    mutation-checked three ways.

**Remaining / v2:**
- **Deploy** — installed manually (bake + copy); `deploy-staging.sh` now lists
  `journal` so the next full deploy installs it too (but that also rolls the
  worker — fine when deploying anyway).

---


A Ratatui terminal app to walk old `trade-control-staging` plans, load them
into TradingView, replay them, and delete once journalled. Keyboard-first,
left→right screen-stack flow.

## Decisions (settled)
- **Stack:** Ratatui + crossterm TUI. New workspace crate `journal`
  (binary `journal`), **env-suffixed exactly like `trade-control` / `tv-arm`**
  — deploy scripts install `journal-staging` / `journal-dev`, and `build.rs`
  bakes `BAKED_ENV_SUFFIX` so `journal-staging` shells out to
  `trade-control-staging` / `replay-candles-staging` (same env). See the
  "Env-suffixing" section below.
- **Data source:** shell out to the `-staging` suffixed CLIs (no HTTP/API
  coupling, no Postgres dep). Env is fixed to *staging* for v1.
- **TV load:** drive tv-mcp (Node scripts under
  `~/Downloads/tradingview-mcp-jackson`, same launcher pattern as
  `scripts/tv_arm_hs.py`) to set symbol + date window in an open TV tab.
  Fires automatically on entering the **Detail** screen (screen 1).
- **Navigation = a LEFT→RIGHT SCREEN STACK, not a two-pane master/detail.**
  `→` pushes deeper, `←` pops back one; `←`×N returns to the list.
- **Divergence (replay-vs-live):** the **Compare** screen exists in the stack
  from v1 so the navigation model is complete, but its *content* is v1 =
  replay report + live timeline shown side-by-side; the actual **diff/
  divergence detection is v2**.
- **NO plan-detail screen in the left→right flow.** The full dump is an
  optional **popup** (a key toggles it over any screen). The handful of facts
  worth seeing always live in a **persistent info bar** (top of the frame).

## ⚠️ Dependency pin: ratatui 0.29, NOT 0.30
The workspace pins `time =0.3.41` (via `tradenation-api`'s reqwest/cookie
constraint — a deliberate pin around a `time 0.3.47` coherence regression;
comment lives in the git `tradenation-api/Cargo.toml`). `ratatui 0.30` needs
`time ^0.3.47` transitively (`ratatui-widgets`, non-optional) → unresolvable.
`ratatui 0.29` only touches `time` behind the optional **calendar** feature, so
`ratatui = { version = "0.29", default-features = false, features =
["crossterm"] }` + `crossterm 0.28` resolves cleanly. **Do not bump to 0.30**
until the workspace `time` pin is relaxed.

## Env-suffixing (mirror `tv-arm`)
- Add package `journal` to `CLI_PACKAGES` and binary `journal` to
  `CLI_BINARIES` in `deploy-lib.sh` — that's all the deploy plumbing needed;
  `deploy-staging.sh` / `deploy-dev.sh` then build + install
  `journal-staging` / `journal-dev` with the env baked in.
- `journal/build.rs`: bake `BAKED_ENV_SUFFIX` from `TRADE_CONTROL_ENV_SUFFIX`
  (copy `tv-arm/build.rs`). At runtime resolve sibling binaries as
  `trade-control-<suffix>` and `replay-candles-<suffix>` (empty suffix → bare
  names for a plain `cargo run`). This is the ONLY coupling to the env; the
  webhook URL is NOT this crate's concern (it never posts directly — it drives
  the already-baked `trade-control-<suffix>` CLI, which owns the URL).

## Info bar — the facts that matter (persistent, top of frame)
Derived from `plan export <id>` JSON (+ the `entered` record's ts from
`plan timeline`). No dedicated screen:

| fact | source in the exported plan |
|---|---|
| **Instrument** | `plan.instrument` (display name via `instrument-lookup`) |
| **Timeframe** | `plan.granularity` |
| **Broker** | `plan.account` / source |
| **Entry mode** | which enter rules are present (by `RuleKind` from basename): `05-enter` only → **normal (break+close+retest)**; `09-enter-qm` (`needs_confirmed`) → **Quasimodo**; **both** → **strategy-v2** |
| **Order type** | `ResolvedEntry` on the enter leg(s): `Market` / `Stop` / `Limit` (BCR leg = stop; QM leg configurable, limit default). Show per-leg for strategy-v2. |
| **Entry timestamp** | `plan timeline` — the `entered` record's `.ts` (Brisbane) |
| **Outcome** | `plan timeline` — final outcome verdict |

## ⚠️ CLI surface is moving RIGHT NOW (another agent)
Another LLM is converting `tv-arm-staging` `--register` / `--plan` /
`--plan-out` / `--replay` from `--flags` into **subcommands**. Implication for
this crate: **never hardcode a flag form in the UI/business layer.** Every
shell-out lives in exactly one function in `cli.rs`; at build time (step 1)
run each `-staging` command's `--help` to pin the *then-current* invocation and
keep them isolated so a later flag→subcommand flip is a one-line change per
wrapper. The commands this crate calls are on `trade-control-staging` (`plan
list/timeline/export/delete`) and `replay-candles-staging` (`--plan`); confirm
these against `--help` before wiring — do not assume the shapes above survive
the other agent's refactor.

## Wire contracts (verified in cli/src/bin/trade_control.rs)
- `trade-control-staging plan list --include-all --yaml`
  → YAML sequence; per-plan keys: `trade_id, account, instrument, shadow,
  phase, rules, fired, archived_at`.
- `trade-control-staging plan timeline <id> --json`
  → `PlanTimeline { records: [RequestRecord], ticks: [TickBundle] }`
  (`trade_control_core::recording`). `RequestRecord.outcome` is the short
  verdict string (`"entered"`, `"rejected: missing-prep"`, …), `.ts`,
  `.logs[]`. Outcome box = derived from these records.
- `trade-control-staging plan export <id>`
  → single-line flow JSON of the bare `TradePlan` (re-registerable). Carries
  `trade_id, instrument, granularity, armed_at`. This is the exact JSON
  `replay-candles --plan` consumes.
- `replay-candles-staging --plan <file>` → replay report on stdout.
- `trade-control-staging plan delete <id>` → deletes plan + engine state
  (idempotent).

## Navigation model — a left→right screen stack

A per-plan **depth** cursor (0 = list). `→` pushes deeper, `←` pops. The
deepest screen reached is remembered per plan so `d` can gate on it (see
delete rules).

| depth | screen | what happens on ENTER (push) |
|---|---|---|
| 0 | **List** | — (the plan picker) |
| 1 | **Timeline** | fetch + render `plan timeline`; fetches `plan export` to fill the info bar. Does **not** touch the TV chart — press `l` |
| 2 | **Replay** | run replay, render the report |
| 3 | **Compare** | replay report ‖ live timeline (v2: computed diff) |

- The **info bar** (top) is always visible from depth 1 on, showing the facts
  table above for the open plan. There is **no** Detail screen; the full plan
  dump is an optional **popup** (`i`) over whatever screen you're on.
- `→` (or `n` = next / drill) pushes to the next screen; a screen's side-effect
  (TV load + info-bar fill, timeline fetch, replay run) fires **once** on first
  push and is cached.
- `←` pops one screen. From depth 1, `←` returns to the list. `←`×N unwinds to
  the list from anywhere.
- On the **List** screen, `↑`/`↓`/`j`/`k` move the selection.

### Screen sketches
Info bar (top) is drawn on every non-list screen:
```
┌ NZD/CHF  H1  TradeNation │ strategy-v2 (BCR stop + QM limit) │ entry 08:00 Bris │ ✗ SL ┐
```

```
LIST (depth 0)                     TIMELINE (depth 1)          REPLAY (depth 2)      COMPARE (depth 3)
┌ Plans ───────────┐               [info bar]                  [info bar]            [info bar]
│> nzdchf-hs-3  ✗  │   Enter/→      ┌ Timeline ──────────┐     ┌ Replay report ─┐    ┌ replay ‖ live ─┐
│  eurgbp-hs-1  ✓  │   ───────▶     │ 07:30 ⊙ prep b&c   │     │ (running…)     │    │ v1 side-by-side │
│  gbpusd-mw-2  ⏳ │   (+TV load)   │ 08:00 • enter→ent… │     │ … report …     │    │ v2 diff         │
└──────────────────┘               │ 13:00 • enter→rej… │     │                │    │                 │
 ↑↓ move →/n open q quit           └────────────────────┘     └────────────────┘    └─────────────────┘
                                    ← list   i popup            ← timeline            ← replay
```

- The optional plan-detail **popup** (`i`) overlays the full `plan export` dump
  for when you want everything, not just the info-bar facts.

## Keybindings
| key | action |
|---|---|
| `↑`/`↓`/`j`/`k` | move selection (list screen only) |
| `→` / `n` / `Enter` | push deeper (list→timeline→replay→compare) |
| `←` | pop back one screen (from timeline → list) |
| `/` | **search/filter the list** (list screen only) — see below |
| `l` | (re)load current plan into TradingView — **operator-initiated only** |
| `r` | (re)run replay for current plan |
| `s` | **save fixtures** — capture the fixture grid (`tv-arm --save-fixture`) |
| `c` | **copy** the full current view to the clipboard (not just the visible part) |
| `i` | toggle the full plan-detail **popup** (overlay) |
| `d` / `x` | **delete + done** — confirm modal; **disabled at depth 0** |
| `Ctrl-L` | force a full repaint (recovers from residual screen corruption) |
| `q` / `Ctrl-C` | quit |

### Fixture capture (`s`) — replaced the SQLite journal DB (2026-07-29)

`s` runs `tv-arm-<env> --start <armed_at> [skip flags] --save-fixture
--fixture-name <trade_id> replay`, which reads the chart **once** and writes the
whole grid — **eight cells**: four entry rules (normal / skip-bcr / strategy-v2
/ strategy-v2-qm-market) × news on/off — under `replay-fixtures/`. It was six
before `strategy-v2-qm-market` was added; the status line reports the count that
actually landed rather than restating a number that goes stale.

### Fixture indicator in the info bar (2026-08-07)

The info bar ends with `fixture <n> ✓` or a dimmed `no fixture`, answering "have
I already captured this one?" without leaving the TUI. A finished `s` re-scans
the corpus, so the row flips as soon as the capture lands.

Matching is **instrument + granularity + window start within 24h, nearest
wins** (`journal/src/fixtures.rs`) — not the directory name, and not a stored
id. The reasons are load-bearing and documented in that module's header:

- The **name** only links journal-captured cells (`--fixture-name <trade_id>`);
  tv-arm's own default derives `<instrument>-<granularity>-<date>`, which is how
  111 of the 125 cells on disk are named.
- **`meta.arm.journal_ref`** is the field designed for this (`--trade-ref`), but
  nothing passes it, so it is `null` in every cell.
- **`plan.json`'s `trade_id` is a trap**: each cell is re-armed independently and
  mints its own id, so the corpus holds 111 distinct ids and **zero** of them
  match any live plan (measured against 46 plans, 2026-08-07).

Note the corpus was captured from plans that have since been journalled and
deleted, so **every current plan reads `no fixture`** — that's accurate, not a
bug. The nearest near-miss is Δ1d8h, and it is a genuinely different setup (a
third NZD/CHF plan), so widening the window would create false positives.

It **replaced** the old `s` = "record the outcome to a SQLite journal DB"
action, and `journal/src/record.rs` + the `rusqlite` dependency were deleted
with it. Why: a fixture is JSON on disk that gets **committed to git** — so it's
versioned, diffable, reviewable, and re-runnable offline forever. The DB was a
local gitignored file only `sqlite3`-by-hand could read (its own module docs
admitted querying was "out of scope"). No DB had ever been created, so nothing
was migrated.

Two constraints it inherits from the replay, both load-bearing:

- **The chart must be loaded first.** tv-arm re-arms from whatever chart is up,
  so capturing against the wrong chart freezes the *wrong setup* — a wrong
  answer, not a slow one. `save_fixture_current` parks the request
  (`save_fixture_pending`) and drives the load itself; `apply_job` runs it when
  `LoadTv` completes.
- **The skip flags must match the original plan.** A `--skip-bcr` plan re-armed
  without them gets the full break-and-close-then-retest and pins the wrong
  gates. Read from the stored plan's preps via `BcrPreps::tv_arm_skip_flags`.

`--save-fixture` and `--fixture-name` are **tv-arm** flags, so they must precede
the `replay` subcommand (`cli::save_fixture_args` enforces the order; a test
pins it).

### TV chart loading (`l`) — never automatic
Entering a screen does **not** load the TradingView chart (auto-load removed
2026-07-27): walking the backlog shouldn't yank the live chart around. Press `l`.

The one exception isn't a convenience: **`r` (replay) loads the chart if it
isn't this plan's**, because `tv-arm --start … replay` re-arms from whatever
chart is up — replaying against another plan's chart returns a *wrong answer*,
not just a slow one. So the replay treats the load as a hard precondition and
waits for it (`PlanData.tv_loaded`).

If you press `l` before the plan detail has arrived, the request **parks**
(`App.tv_load_pending`) and runs when the timeline job lands — the detail
carries the broker that fixes the chart's exchange prefix. That flag is what
distinguishes "the operator asked" from "a timeline happened to load"; don't
drop it, or `l` silently no-ops on a not-yet-fetched plan.

On the **Replay** screen the vim/arrow keys scroll the report instead of moving
a selection (`j`/`k`/`u`/`d`/`g`/`G`, PgUp/PgDn/Home/End), so delete there is
`x` only. Same scroll bindings inside the `i` popup.

### Search (`/`)
- `/` opens a one-line prompt under the list; the filter applies **as you type**.
- **Case-insensitive substring** over everything the row shows (`trade_id`,
  instrument, granularity, phase) plus `account` and the word `archived`.
- **Space-separated terms are ANDed, in any order** — `eur h4` and `h4 eur` both
  find the EUR H4 plans. Deliberately not fuzzy: you're usually typing a
  fragment you can see, and fuzzy matching surfaces confusing hits.
- **Separators are interchangeable** (`_`/`/` fold to `-`), so `audcad`,
  `aud-cad` and `AUD_CAD` all match each other regardless of broker spelling.
- `Enter` closes the prompt but **keeps** the filter (a dimmed `filter:` line
  stays visible); `Esc` clears the filter and restores the full list. With a
  filter applied and the prompt closed, `Esc` clears it rather than quitting.
- While typing, every printable key goes into the query — `q`/`d`/`r` etc. can't
  fire their commands. `Ctrl-C` still quits; `↑`/`↓` still move the selection so
  you can pick a row without leaving the prompt.
- The title reports `matched/total`, and clearing the filter keeps you on the
  same plan you had highlighted.
- **Implementation note:** `app.selected` indexes the **visible (filtered)** rows,
  not `plans` — `App::visible()` maps back. Anything new that reads a row must go
  through `current_plan()` / `visible_plans()`, never `plans[selected]`.

### Delete rules
- `d` (alias `x`) means **delete (and "done")** — retire a plan you've finished
  journalling.
- **Guarded:** no-op (with a footer hint) unless the open plan's max depth
  reached is **≥ 1** — i.e. you've drilled in past the list at least once. Can't
  delete a plan straight from the list without looking at it.
- **Always confirms:** opens a modal — `y` deletes (`plan delete <id>`),
  refreshes the list, and returns to depth 0; `n`/`Esc` cancels.

## Crate layout (small modules, no mod.rs)
```
journal/
  build.rs              # bake BAKED_ENV_SUFFIX (copy tv-arm/build.rs)
  Cargo.toml            # ratatui, crossterm, color-eyre, tracing,
                        # tracing-subscriber, tracing-error, serde,
                        # serde_json, serde_yaml, chrono,
                        # instrument-lookup (path)
  src/
    main.rs             # tracing init, terminal setup/teardown, event loop
    app.rs              # App state: plans, list selection, Screen depth,
                        #   per-plan max-depth-reached, info-bar facts,
                        #   popup flag, modal, TV/replay caches
    screen.rs           # enum Screen { List, Timeline, Replay, Compare }
                        #   + push()/pop() depth transitions + delete-guard
    cli.rs              # subprocess wrappers: list_plans/timeline/export/
                        #   replay/delete. ONE fn per command — the only place
                        #   that knows flag-vs-subcommand form AND the env
                        #   suffix (trade-control-<suffix>, replay-candles-<suffix>).
    plan.rs             # PlanRow (list) + PlanDetail (export JSON) parsing;
                        #   entry-mode classifier (normal/QM/v2) + order type
    timeline.rs         # PlanTimeline parse + outcome + entry-ts + event lines
    tv.rs               # tv-mcp launcher: symbol via instrument-lookup, window
                        #   [armed-1d, armed+2d], set-symbol call
    ui.rs               # render(): closures/derived first, then info bar +
                        #   dispatch on Screen + popup/modal overlays
    ui/infobar.rs       # persistent top facts bar
    ui/list.rs          # list screen
    ui/timeline.rs      # timeline screen
    ui/replay.rs        # replay-report screen
    ui/compare.rs       # compare screen (v1 side-by-side; v2 diff)
    ui/popup.rs         # `i` full plan-detail overlay + delete confirm modal
    keys.rs             # KeyEvent → Action mapping
```

## Concurrency
- Shell-outs (`timeline`, `export`, `replay`) can be slow. v1: run them
  **synchronously with a "loading…" flash** in the status line (simplest,
  <600 lines). If replay latency annoys, v1.1 moves replay to a spawned
  thread posting the result back over an mpsc channel. (Note in code, don't
  build the async path in v1.)

## Build steps (each ends green: tests + clippy + fmt)
1. **Scaffold + env-suffix + pin the CLI surface** — `cargo new journal` in the
   workspace, add to root `members`, `cargo add` deps, add `build.rs` baking
   `BAKED_ENV_SUFFIX`. Register `journal` in `deploy-lib.sh`
   (`CLI_PACKAGES`/`CLI_BINARIES`). Run `trade-control-staging plan --help` and
   `replay-candles-staging --help`, **record the exact current invocations in
   `cli.rs` doc-comments** (flags may already be subcommands — the other agent
   is mid-refactor). Stub `main.rs`: boot Ratatui, draw "journal", quit on `q`.
2. **cli.rs + plan.rs** — `list_plans()` shells `plan list --include-all
   --yaml`, parses to `Vec<PlanRow>`. `plan.rs` also parses `plan export` JSON
   into `PlanDetail` with the **entry-mode classifier** (normal/QM/v2 from which
   enter rules are present) and **order type** (`ResolvedEntry`). Unit-test both
   parsers against captured samples. `--dump` prints to stderr, no UI.
3. **List screen** — render plans, `↑↓`/`j`/`k` selection, `q` quit.
4. **Screen stack + info bar** — `screen.rs` push/pop (`→`/`n`, `←`). On first
   push to Timeline: fetch `plan timeline` + `plan export`, fill the info bar
   (instrument/tf/broker/entry-mode/order-type/entry-ts/outcome) and render the
   timeline. Track per-plan max-depth.
5. **Replay screen** — depth 2 runs `plan export`→temp→
   `replay-candles-<suffix> --plan`, renders report. Loading flash.
6. **Compare screen** — depth 3: v1 shows replay ‖ timeline side-by-side
   (diff = v2, stub the diff fn).
7. **Delete** — `d`/`x`: guard on max-depth ≥ 1, confirm modal, `y` runs
   `plan delete`, refresh + return to list; `n`/`Esc` cancel.
8. **Detail popup** — `i` toggles a full `plan export` dump overlay.
9. **TV load** — `tv.rs`: `instrument-lookup` → TV symbol, `[armed-1d,
   armed+2d]`, drive tv-mcp set-symbol. Auto-fires on timeline push; `l`
   re-fires. If tv-mcp set-symbol isn't cleanly scriptable, fall back to
   `xdg-open` of a TV chart URL and note it.
10. **Polish** — footer hints, error surfacing (failed shell-out shows the
    CLI's stderr in a footer, never a panic), README section, commit+push,
    advance parent submodule pointer.

## Out of scope (v1)
- replay-vs-timeline divergence **diff** (Compare screen exists; diff is v2)
- env switching (staging only; dev/prod later)
- editing plans, arming, or any write beyond `plan delete`
- mouse support

## Open detail to confirm during build
- Exact tv-mcp set-symbol invocation (read `scripts/tv_arm_hs.py`'s launcher +
  the Node scripts in `~/Downloads/tradingview-mcp-jackson`). If set-symbol
  isn't cleanly scriptable, fall back to `xdg-open` of a TV chart URL for v1
  and note it.
