# TODO — `journal` TUI (trade-journalling operator tool)

## STATUS 2026-07-23 — v1 SHIPPED, verified live on staging
Commits `042fdd7` (scaffold), `3912950` (TUI), `f6612b8` (fired-rule fix).
Installed as `journal-staging` / `journal-dev`. All screens proven end-to-end
in a real terminal (tmux) against the live staging worker: List → Timeline
(info bar: `AUD/CAD · h1 · short │ normal (break+close+retest) (BCR stop) │
outcome`) → Replay (full `replay-candles` report) → Compare (replay ‖ live
side-by-side). Delete guard blocks unopened plans; confirm modal, `i` detail
popup, and `←`-unwind all work. 13 tests (incl. 2 TestBackend render tests).

**Remaining / v2:**
- **Compare diff** — currently side-by-side only; compute + highlight the
  replay↔live divergences (the stated bug-hunting goal).
- **TV auto-load on Timeline push** — wired via `load_tv` (`l` key, replay
  `--annotate`) but NOT yet auto-fired on the Timeline push; `run_screen_effect`
  has a TODO marker. Decide: auto-annotate is slow (pulls candles), so maybe
  keep it on the explicit `l` key rather than auto.
- **Deploy** — installed manually (bake + copy); `deploy-staging.sh` now lists
  `journal` so the next full deploy installs it too (but that also rolls the
  worker — fine when deploying anyway).
- Async replay (spawn + channel) if the synchronous run's freeze annoys.
- Parent submodule pointer bump after this lands on `main`.

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
| 1 | **Timeline** | fetch + render `plan timeline`; **auto-loads TV** + fetches `plan export` to fill the info bar |
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
| `l` | (re)load current plan into TradingView (auto-fires on timeline push) |
| `r` | (re)run replay for current plan |
| `i` | toggle the full plan-detail **popup** (overlay) |
| `d` / `x` | **delete + done** — confirm modal; **disabled at depth 0** |
| `q` / `Ctrl-C` | quit |

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
