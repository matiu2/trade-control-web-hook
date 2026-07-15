# trade-control-web-hook — orientation for Claude

Two layers live in this repo:

1. **Rust worker + CLI** (`src/`, `core/`, `cli/`, `broker-oanda/`) — the
   worker that receives signed TradingView alerts and the `trade-control` CLI
   that signs them. The README covers the wire format, actions
   (`enter`/`prep`/`veto`/...), and CLI subcommands in depth. **Runtime note:
   this is now a native local Postgres worker (`trade-control-worker`), NOT a
   Cloudflare Worker** — see the environments section below.
2. **Chart-driven Python tool** (`scripts/tv_arm_hs.py`) — reads a
   TradingView head-and-shoulders chart via tv-mcp and produces the full
   alert bundle for one setup by shelling out to `trade-control build-trade
   --from-file`. The README has a section on it; this file is the
   "stuff a future Claude will get bitten by" deeper note.

Read the README first for the user-facing story. This file is for hazards.

## Runtime: fully local, no Cloudflare (2026-07)

**Cloudflare is fully retired.** Both environments run as **native
Postgres-backed workers on the local desktop** — the `trade-control-worker`
binary (axum HTTP + tokio scheduler), backed by a local PostgreSQL instance,
migrating its schema on boot. No Cloudflare Worker, no KV, no R2, no `wrangler`
anywhere in the live path. What was KV is now Postgres rows; what was R2
recording is Postgres / the `ticks/` prefix; what were `wrangler secret`s are
worker process env vars (`SIGNING_KEY`, `ADMIN_KEY`, `OANDA_TOKEN`).

**The Oracle Cloud host is still the long-term target but not available yet.**
Its Autonomous DB (region `uk-london-1`) is live and the Postgres→Oracle port
is de-risked (`SPIKE-oracle-findings.md`), but the OKE compute has **0 nodes**
(London out of ARM capacity). Until a node lands, **local is the deploy
target** — not a permanent state, just where we run this week. The backend is a
compile-time Cargo-feature swap (`postgres` XOR `oracle`), so Oracle is a
rebuild, not a rewrite (`SCOPING-oracle-db-swappable.md`).

## Branches are environments — know which one you're on

Each git **branch is a deploy environment**. Each carries its own worker
config file under `~/.config/trade-control/` (its own bind port + Postgres
database), and the deploy scripts branch-guard so you can't cross wires.

| branch | environment | worker | port | Postgres role / DB | CLIs | who uses it |
|---|---|---|---|---|---|---|
| `main` | **dev** | `trade-control-worker` (local) | `8787` | `candle_cache` / `trade_control_dev` | `*-dev` | coding branch; **worker not currently driven** |
| `staging` | **staging (demo)** | `trade-control-worker` (local) | `8788` | `tc_staging` / `trade_control_staging` | `*-staging` | live demo trading — **updated as we go, no stable freeze yet** |
| `prod` | **prod (real money)** | — | — | — | `*-prod` | **not stood up yet** — first promotion target (Oracle) |

One local PostgreSQL server (`:5432`), two databases, two worker processes.
Dev stays on the `candle_cache` role (avoids table-ownership churn); staging
has a dedicated `tc_staging` role + `trade_control_staging` DB. The suffixed
CLIs bake their worker URL (`http://127.0.0.1:8787|8788`) at compile time.

Current working split (2026-07): **there is no stable staging yet, and `dev`
is not in use.** We code on `main` and merge to `staging` continuously —
`staging` is **updated as we go**, redeployed whenever a fix lands. All the
real work (coding + the live demo worker) happens on `main` → `staging`; the
`dev` worker exists but nothing is being driven through it right now.

So, for now, **do redeploy `staging` freely** as fixes land — the "don't
redeploy mid-week" caution below applies only *once the promotion clock has
started*, which it hasn't. The promotion gate is a **two-stage clock, not yet
running**:

1. **A week of `staging` with no bugs found.** Keep fixing + redeploying until
   a full week passes without a new bug surfacing.
2. **Then a second week with no changes at all.** Only after stage 1 do we
   freeze `staging` and let it run a full week *unchanged* + profitable. That
   frozen-and-profitable week is what promotes to `prod`.

Until stage 2 begins, treat `staging` as a fast-moving demo, not a frozen
release — merge to it and redeploy the moment a fix is green.

**Deploy** with the per-environment scripts (they rebuild + install the
matching suffixed CLIs and restart the local worker; neither calls `wrangler`):

```sh
git checkout main    && ./deploy-dev.sh       # dev  → :8787
git checkout staging && ./deploy-staging.sh   # staging → :8788
# ./deploy-live.sh is added at the first prod promotion (Oracle).
```

Or run a worker directly (long-running, both keys + OANDA token required):

```sh
SIGNING_KEY="$(tr -d '[:space:]' < ~/.config/trade-control/key.hex)" \
ADMIN_KEY="$(tr -d '[:space:]' < ~/.config/trade-control/admin-key.hex)" \
OANDA_TOKEN="$your_token" \
  ./target/release/trade-control-worker ~/.config/trade-control/<local|staging>-worker.toml
# health: curl 127.0.0.1:<port>/health → ok
```

⚠️ These are foreground/`nohup` processes — a **reboot kills them** (a
Hyprland/compositor crash does not). No systemd unit yet; restart manually
after a reboot.

The scripts branch-guard, so they refuse to deploy from the wrong branch.
`deploy-lib.sh` holds shared logic; the per-env wrappers hold only the
branch + URL.

### Per-environment CLIs (`trade-control-staging`, `tv-arm-staging`, …)

The CLIs are installed under **suffixed names** with the environment's
worker URL **baked in at compile time** (`build.rs` →
`cargo:rustc-env=BAKED_WEBHOOK`, fed from `TRADE_CONTROL_WEBHOOK` by the
deploy script). So `trade-control-staging status` hits the staging worker
with no env var or `--endpoint` flag. Endpoint precedence is
`--endpoint` > `TRADE_CONTROL_ENDPOINT` env > baked default.

**Do not export `TRADE_CONTROL_ENDPOINT` globally** (e.g. in `~/.zshrc`) —
it overrides every suffixed binary's baked URL and silently points them all
at one worker. The unsuffixed `trade-control` / `tv-arm` / `tv-news`
binaries have been removed; use the suffixed ones.

This is a **Cargo workspace** (root `Cargo.toml`; `cli`, `tv-arm`,
`tv-news` are members → shared `./target/`). The parent repo's CLAUDE.md
saying "NOT a workspace, no root Cargo.toml" is about the *outer*
trading-libraries repo and does **not** apply to this submodule. Build CLIs
with `cargo build -p <pkg>` (note `cli`'s package is `trade-control-cli`,
binary `trade-control`).

### Promotion (staging → prod), the upcoming `prod` branch

`prod` doesn't exist yet, and the promotion clock hasn't started (see the
two-stage gate under "Branches are environments"): first a week of `staging`
with no bug found, *then* a second week frozen + profitable. When that frozen
week completes, `staging` gets merged into a new `prod` branch with its own
worker config (its own port + Postgres database, or the Oracle DSN once Oracle
compute lands), and a fresh `staging` is cut from `main`. Prod is a clean new
env with its own worker process + database, *not* a rename of an existing one.
`deploy-live.sh` (added at that point) targets it; `main`/`-dev` and
`staging`/`-staging` keep their own workers. Prod is the **first Oracle
promotion target** — if OKE compute is available by then, prod is the env that
runs against the Oracle Autonomous DB (compile the worker with the `oracle`
feature); otherwise it stands up locally like the others until Oracle lands.

## Things the README doesn't shout

### "retry" / `max_retries` does NOT mean retrying failed placements

This naming has bitten more than one debugging session. `max_retries`,
the "retry gate" in `src/retry_gate.rs`, the `EntryAttempt` rows, and
the `is_retry_fire_seen` / `mark_retry_fire_seen` KV keys are all
**multi-shot re-entry** mechanisms: place → fill → close (typically
at SL) → a fresh signal bar arrives in the same alert window → place
again, up to `max_retries` total placements.

What "retry" is **not**:

- Not a retry of broker placement errors. A failed `place_order`
  returns 502 and does **not** mark the id seen — the next fire of
  the same alert body is allowed through (see "Replay protection
  scope" below). This was different before the 2026-06 fix; older
  worker versions wrote `mark_seen` on every dispatcher outcome
  including failures, which silently broke within-window legitimate
  refires.
- Not a retry of pre-broker rejections (veto, cooldown,
  `allow_entry` script failures). Those don't burn an `EntryAttempt`
  slot at all, and as of the same 2026-06 fix they don't poison the
  intent id either.
- Not a way to refire the same alert payload after a *successful*
  entry. Top-level intent-id dedup still applies on `Ok` outcomes —
  multi-shot needs distinct intent ids per fire (which `build-trade`
  mints for multi-shot setups, not for single-shot).

### A multi-shot enter must NOT retire the cron-engine plan

In the cron-engine model a `FireMode::Once` enter set `Phase::Done`
the moment it fired, and the cron archives + clears any `Done` plan
(`src/cron/engine.rs`, `persist_plan_state`). For a *single-shot*
enter that's correct. For a **multi-shot** enter (`max_retries > 0`)
it silently broke re-entry: the plan that would fire attempt #2 was
gone after the first fire, so place → fill → close → re-enter never
got a second bar. The live worker would enter once and never
re-enter even though the operator opted into multi-shot. Caught by
the replay on NZD/CHF 2026-06-19 (07:30 short → SL, expected 13:00
re-entry never fired).

Fix (commit `83333fa`, `engine/src/evaluate.rs::evaluate_entry`): a
`Once` enter only transitions to `Phase::Done` when it is *also*
single-shot (`max_retries == Tunable::Static(0)`). Anything else —
including a script-resolved cap — is treated as multi-shot, fires
this bar, and **stays in `Phase::AwaitEntry`**: the plan survives,
its vetos keep ticking, and the next golden signal bar fires the
enter again. The placement cap is **not** the engine's job — it's
the worker's retry gate. The plan still retires the normal way (a
terminal `close-positions` veto, `trade-expiry`, or the enter's
`not_after` window closing). New test
`multi_shot_pine_enter_fires_but_stays_await_entry`; single-shot
behaviour is byte-identical (all prior tests use `Static(0)`).

The retry gate itself **moved to `core`** (commit `edef1ea`):
`trade_control_core::retry_gate` (was `src/retry_gate.rs`, worker
bin, wasm-only). It was already generic over `<B: Broker, S:
StateStore>`; the only worker coupling was the `rlog!` / `rlog_err!`
recording-buffer macros, now plain `tracing::info!` / `error!`
(wasm-safe). Both consumers now call
`trade_control_core::retry_gate::evaluate` with their own `Broker` —
the worker its real TradeNation/OANDA broker, the offline replay a
fake broker that approximates a prior attempt's state by simulating
it against the candle window. One async gate, swappable broker, no
decision duplication. (Stale `src/retry_gate.rs` paths in the prose
above and a comment at `src/lib.rs:1186` predate this move.)

### Replay protection scope

The intent-id seen index (`is_seen` / `mark_seen`) covers two
distinct cases with different semantics — keep them straight:

1. **Entry-bearing actions** (`Enter`, `Close`, `Invalidate`,
   escalated `Veto`) — `mark_seen` is written **only on
   `ActionResult::Ok`**. `Failed` (broker error → 502) and every
   flavour of `Rejected` (gate failure, validation, state error)
   are logged via `console_log!` / `tracing::info!` for post-mortem
   visibility but deliberately do **not** consume the intent id.
   The next fire of the same alert body is allowed through.
   Implemented by `record_dispatcher_outcome` →
   `seen_decision` in `src/lib.rs`.

2. **Control actions** (`prep`, level-1 `veto`, `pause`, `resume`,
   `clear-prep`, `clear-veto`, `status`, `news-start`, `news-end`,
   `unlock`) — `mark_seen` is written on **every** completion via
   the `record_seen` helper. These are state-set ops where
   idempotency is legitimate (a replayed `prep` message shouldn't
   double-refresh the TTL).

**Why this matters.** A real incident on 2026-06-02 (CHF/JPY): an
`enter` alert fired 6 times in a 9h window. Fire 4 reached the worker
post-parse-fix, was correctly rejected with `rejected: missing-prep
(break-and-close)` because the prep alert had not fired yet — and
the old "every outcome marks seen" rule poisoned the intent id for
the remaining ~47h of the alert window. Fires 5
(`signal_confirmed=1`, the entry the operator wanted) and 6 both
409'd on `is_seen` at `src/lib.rs:154` before reaching the
`allow_entry` script gate. Operator entered the trade manually.

Pattern an unwary refactorer might fall back into: "let's mark seen
on every dispatcher outcome too, for visibility in `status`." Don't.
The cost of poisoning legitimate retryable rejections far outweighs
the visibility gain — and gate rejections are already visible in the
worker's `tracing` logs (journalctl for the systemd unit) via the
`log_skip` line.

If you find yourself looking at a 409 on an `enter` and wondering
"but the prior fires all failed — why didn't the retry gate let it
through", remember: the *seen-id replay check* and the *retry gate*
are two different things. With the 2026-06 fix in place, failures
no longer poison the id at all — so a 409 on an `enter` now means
**a prior fire of this id succeeded** (entered, closed, etc).
Before that fix, a 409 could just mean "we logged some prior
non-Ok outcome." If you're still seeing surprising 409s post-fix,
look at the *latest* recorded outcome via `trade-control status` to
see what the prior successful fulfilment actually was.

### `veto_on_reversal`: a rejected enter may be a self-inflicted reversal veto

Added 2026-06 (worker v13). The reversal-close (`close` with a price
window) can carry an opt-in `veto_on_reversal: true`. When that
close's gate passes, the worker *also* writes a veto under the fixed
name **`reversal`**, scoped to the intent's `trade_id`. A later
`enter` for the same setup then gets rejected by the ordinary
`is_vetoed` gate — **not** by replay-dedup and **not** by the retry
gate.

The debugging trap: an `enter` is rejected (`rejected: veto-active
(reversal)` in the logs / `412`), the prep gates all look satisfied,
and no prior `enter` succeeded — so neither the seen-id check nor the
retry gate explains it. The answer is a `reversal` veto written by an
*earlier reversal-close fire in the same window*, exactly the
pre-entry case the flag exists for. This is **working as designed**
when the flag is on. Confirm via `trade-control status` (the
`reversal` veto shows under the trade_id) and, if you want the trade
anyway, clear it with `trade-control clear-veto <instr> reversal
--trade-id <tid>`.

Key facts a refactorer must preserve:
- **It takes two halves.** The worker only checks veto names the
  `enter` lists in its `vetos`. So the close writing the `reversal`
  veto does nothing on its own — the matching `05-enter` must also
  carry `reversal` in `vetos`. `build_trade_from_spec` adds both
  together (gated on `veto_on_reversal && !sr_reversal_ranges.is_empty()`).
  If you hand-craft a close with `veto_on_reversal: true` but no
  matching enter half, you write a veto nothing reads.
- The veto name is the fixed string `reversal`
  (`trade_control_core::intent::REVERSAL_VETO_NAME` — single source of
  truth shared by the worker write side and the CLI enter-builder).
  `status` / `clear-veto` key on it.
- It's written on **every** gate-pass, not only pre-entry — so
  post-entry it harmlessly blocks a re-entry for the rest of the
  window. Don't "optimise" this into a flat-only write without
  re-reading the multi-shot interaction (a winning-then-reversing
  trade's close would then permit a fresh entry the operator may not
  want).
- It is **StopNextEntry-only** — it must never escalate to closing a
  position. The close the intent already performs is the only
  position action. (See the `veto_close_only_when_thesis_invalidated`
  rule.)
- Default OFF, experimental. The decision logic is the pure
  `reversal_veto_plan()` helper (KV-free, unit-tested); the KV write
  is a thin wrapper.

### Intrabar crosses fire on any straddle (high and low opposite sides), not the close

**Updated 2026-07-03 (`engine`).** The `BarEvent::Intrabar` arm is now a **pure
straddle**: the bar's high and low must sit on *opposite* sides of the level,
with the directional wick reaching at/through the line. It is **both open- and
close-agnostic** — this loosened the earlier "wick from the open side" rule
(2026-07-01, `f231629`) by dropping the `open`-side guard. The engine's
`level_crossed` (`engine/src/evaluate.rs`) arm:

- `Up`     ⇒ `high >= level + buffer`  (low ≤ level guaranteed by the straddle)
- `Down`   ⇒ `low  <= level - buffer`  (high ≥ level guaranteed by the straddle)
- `Either` ⇒ any straddle (`low <= level <= high`) — unchanged

The intuition (operator's framing): on a tick timeline a bar whose *range* spans
the level traded on both sides of it, which is enough to count as a touch/cross
regardless of where it opened or closed. The previous rule additionally required
the bar to have *opened* on the far side (`Up ⇒ open <= level`); a bar that
opened on the near side, wicked through, and came back was rejected. The straddle
rule fires it. (The even-older rule required a confirming **close** — that was
already dropped 2026-07-01; the retest tap-and-bounce bug it fixed was AUD/JPY
iH&S long 2026-06-29, a 6pm bar that wicked below the descending neckline and
closed back above.) The directional `buffer` (`cross_buffer_pct`) still applies to
the cross-side wick so a one-tick graze doesn't trip it. See
`[[intrabar_cross_reads_wick_not_close]]`.

**`BarEvent::OnClose` now reads the ORIGIN side, not the prior close (2026-07-15).**
An OnClose cross used to be an **edge detector** (`prev_close < far_edge &&
close >= far_edge`) — it fired only on the bar that made the below→above (or
above→below) *transition*. That lost a genuine break-and-close whenever the
transition bar was suppressed (a spread-hour rubbish candle) or the plan armed
already-past the line: the next clean bar had a `prev` already on the far side,
so `prev < far_edge` was false and it never stamped. Stranded EUR/GBP & GBP/USD
setups in `AwaitBreakAndClose` forever.

The rule is now **origin-side / settled-close** for the latching consumers: the
**origin** (the open of the first bar the rule ever saw — the arm-time bar in
practice, stored once in `PlanState.origin_open`) fixes which side the plan
started on, and **any bar that CLOSES on the far side of the line from the
origin fires**, whether or not the transition happened on that bar. Operator's
model: "we know which side we started on; the first bar that *closes* across is
the break." Robust to a suppressed/skipped transition bar. The "must reach the
far *zone* edge" half is retained, so the NAS100 "zone of the line" fix stands
(a close that only dips into the buffer zone is not a break). Applies to **all
latching OnClose rules**: `03-prep-break-and-close`, `04-prep-retest` (when
OnClose), the `too-high`/`too-low` invalidation caps, M/W abort, drawn lines.

**Exception — entry OnClose crosses keep EDGE semantics.** The strategy-v2 stop
/ Quasimodo-limit `05-enter`/`09-enter-qm` OnClose crosses (`HorizontalCross …
on_close` with `action: enter`) still fire once **per transition**, reading the
prior close — a multi-shot entry cross must not re-place an order on every
settled far-side bar. `fire_rule` picks the mode by `rule.intent.action`
(`OnCloseRefs { settled: action != Enter }`); the `Intrabar` arm is unchanged
(reads the wick). See `[[break_close_edge_detector_misses_already_above]]`.

**Retest zone fattens over time, SCALED BY THE NECKLINE'S SLOPE (2026-07-15).**
The `04-prep-retest` cross (only the retest, not other intrabar consumers)
carries a **near-side tolerance that grows with bars since the break-and-close,
at a rate set by the neckline's slope**: `tolerance(N) = (N-1) ×
plan.retest_atr_step × |neckline slope, price per bar|`, where `N` counts bars
after `break_close_at` (first = 1 → tolerance 0, must reach the line). The slope
is measured in the engine's **bar-index** space (`neckline_slope` → `bar_index_at`,
matching `line_price_at`, so a session gap doesn't inflate it). A **horizontal
neckline has slope 0 ⇒ tolerance 0 forever** — a flat neckline is an exact price
level and must be retested precisely; a steeper neckline fattens the band faster.
This is the slope-scaled form of the earlier ATR-only rule (`(N-1) × step × ATR`,
2026-07-03): algebraically it's `(N-1) × step × ATR × (|slope|/ATR)` so the ATR (a
volatility proxy) **cancels** — but the ATR is still computed as the calibration
unit and guards the degrade path below. Stricter than a textbook ATR-band (which
keeps a band even on a flat line): deliberate, so horizontals stay exact. A wick
that comes *within* the tolerance of the line stamps the retest even without
reaching it. Lives in `stamp_retest` → `retest_tolerance` + `neckline_slope` +
`retest_crossed` (`engine/src/evaluate.rs`); the signed field is
`TradePlan.retest_atr_step` (default `DEFAULT_RETEST_ATR_STEP = 0.075`, tv-arm
`--retest-atr-step`). If the ATR can't be computed (window shorter than
`atr_length_for(granularity)`), `retest_tolerance` **degrades to 0.0 (strict
must-reach-the-line)** and `warn!`s — it does **not** panic. It used to
`.expect()`-panic on the theory that the window is always warm past ATR warmup
by the retest phase; that theory was false (`detector_window_for` only reaches
back to the earliest trendline anchor, so a trendline-only / M-W plan with
recent anchors can present a short window) and the panic unwound the whole
shared `tc-scheduler` thread — silently freezing EVERY plan's cron tick for
~17h (staging incident 2026-07-14 04:32Z). `engine_tick_loop`
(`worker/src/scheduler.rs`) now also isolates each tick via `run_isolated`
(`spawn_local` + `JoinHandle` panic-catch) so one bad plan can never kill the
scheduler again. This *loosens*; the separate `cross_buffer_pct` *tightens* and
still governs the non-retest crosses.

**Exception — the literal `too-high` cap reverted to close-confirm (2026-07-01).**
The engine semantics above are unchanged; what changed is which `BarEvent` the
short-side invalidation cap is *built* with. `invalidation_or_pcl_trigger`
(`tv-arm/src/trade_plan_build.rs`) now emits the short `too-high` cap as
`HorizontalCross { dir: Up, bar: OnClose }` — a bar must **close** above the cap
to invalidate; an intrabar spike above that closes back below does **not**. This
is a deliberate *asymmetry* (operator call): only the literal `too-high` name
reverted. The long-side invalidation floor (`too-low`, `dir: Down`) stays
**intrabar (wick)** — a low wicking below the floor invalidates with no close
required. Tests: `builds_hs_short_rules_with_correct_triggers` (asserts
`OnClose`), `ihs_long_too_low_invalidation_stays_intrabar_wick` (asserts the
mirror stays `Intrabar`).

Consumers of the intrabar arm, all now pure-straddle (high/low opposite sides;
for a **short**, mirror for long):

- **`too-high` = invalidation** (drawing-bound horizontal at the shoulder cap).
  **Reverted to `OnClose` — see the exception above.** The short cap is
  close-confirmed; only the long-side `too-low` invalidation floor uses the
  intrabar arm (`HorizontalCross { dir: Down, bar: Intrabar }`).
- **`too-low` = pcl-exhausted** (computed fib, ~80% to TP).
  `PriceValueCross { dir: Either, bar: Intrabar }` — `Either`, so **any
  straddle** aborts. Unchanged: if a short ran 80% to TP without us, a wick
  alone is reason enough.
- **`04-prep-retest`** — `TrendlineCross { dir: Down (long) / Up (short),
  bar: Intrabar }`. The retest of the neckline: long = open above the descending
  neckline, low wicks below. This is what the fix unblocked.
- **M/W cancel / overshoot vetos** (`mw_price_trigger`) — same intrabar arm.

These levels are also baked onto the enter as continuous `entry_level_vetos`
(see Bug #12 / `[[bug12_at_entry_level_vetos]]`) — `is_past`-inclusive and
independent of this cross-guard.

**Cross-depth buffer (built).** A tunable cross-depth buffer so a one-tick graze
doesn't trigger — plan-level signed `TradePlan.cross_buffer_pct`, default
**0.02%** of the line price. A directional **intrabar** `Down`/`Up` cross must
pierce ≥`pct%` of the line price past the line (`Either` and `OnClose` ignore
it). Calibrated on the AUD/JPY iH&S 2026-06-29 (0.0 = −1.43R, 0.02 = +0.57R,
0.1 = starved). See `[[cross_buffer_pct]]`. Follow-up: a `tv-arm
--cross-buffer-pct` flag to override the arm-time default per-trade (not built).

### tv_arm_hs.py: server-side trendline-cross eval is anchor-bounded

Burned a lot of time on this. When you POST a `create_alert` with
`tool: "LineToolTrendLine"`, TV's server only evaluates price crossings
inside `[base_time + offset1*resolution, base_time + offset2*resolution]`
unless you set `extend_forward: true` in the payload. `stateForAlert()`
on the drawing returns `extendForward` based on the *drawing's own*
extension property — which is almost always `false` for an H&S neckline.
**Always force `extend_forward: true` for `LineToolTrendLine` alert
payloads.** Horizontal- and vertical-line alerts are unaffected.

### tv_arm_hs.py: the chart-side `_hasAlert` binding is a red herring

The "link icon" you see on a drawing when you create an alert via TV's
GUI comes from `LineDataSource.setAlert(alertId)` being called locally,
which writes `_alertId` onto the shape and registers a client-side
subscription. Programmatic creates can't easily replicate this — the
alerts facade is module-private and racing it via polling never wins.

This doesn't matter for alert *firing*. Server-side eval works fine
without the binding. If a future investigation says "the link icon is
missing", the answer is "yes, that's expected; don't fix it." Only
chase the binding if alerts genuinely aren't firing — and if so, look
at the *geometry* first (see above), not the binding.

### tv_arm_hs.py: TP geometry

Take-profit price is computed as `2 × neckline − head` from the fib's
two endpoints. Symmetric reflection through the neckline. This is
independent of which fib levels are visible. The user draws the fib
spanning head → neckline; the script does the reflection. If the user
draws the fib differently (e.g. shoulder → neckline, or with both
endpoints inside the range), the formula breaks. Heads up.

### Submodule registration

This directory is treated as a submodule of `trading-libraries` (parent
repo holds a `160000` gitlink to a commit here), but it is **not**
registered in the parent's `.gitmodules`. So:

- `git submodule status` in the parent **doesn't show this repo**.
- Updating: commit + push *inside* this repo first, then in the parent
  `git add trade-control-web-hook && git commit && git push` to advance
  the pointer.
- **Always advance the parent pointer after merging to `main` here.** A
  merge to this repo's `main` is not "done" until the parent gitlink is
  bumped and pushed — otherwise the parent still points at the old commit
  and a fresh `trading-libraries` checkout gets stale code. So the tail of
  every merge-to-main is:
  ```sh
  cd /home/matiu/projects/trading-libraries
  git add trade-control-web-hook && \
    git commit -m "bump trade-control-web-hook: <what merged>" && git push
  ```
  Do this immediately, same as the commit+push+tag-by-default rule — don't
  wait for the user to ask.
- The parent's `CLAUDE.md` (long architectural doc) lists this repo
  under "regular directories" — that note is outdated; treat it as a
  submodule.

### Build-trade pipeline contract

`tv_arm_hs.py` writes a `trade.yaml` to a temp dir and calls
`trade-control build-trade --from-file <trade.yaml> --key-file <key>
--output-dir <dir>`. The Rust side mints `trade_id`, writes
`manifest.yaml`, and emits 5 signed alert YAMLs with fixed basename
ordering:

```
01-veto-<too-high|too-low>
02-veto-trade-expiry
03-prep-break-and-close
04-prep-retest
05-enter
```

The Python script maps these basenames to drawing roles by prefix.
If you rename a basename in the Rust pipeline, update
`build_alert_spec()` in the script to match.

### Risk and dry-run plumbing

`TradeSpec` carries both `risk_amount: Option<f64>` and `dry_run: bool`
since 2026-05. The enter-alert builder honours `risk_amount` over
`risk_pct` when set (the worker validator rejects both), and propagates
`dry_run` only onto the enter intent — vetos and preps never carry it.

On the Python side: `--risk-amount` adds `risk_amount: <n>` to the spec;
`--broker-dry-run` adds `dry_run: true`. The Python `--dry-run` flag is
unrelated — that one short-circuits before any POST to TradingView.

### Pip size is baked into the enter intent, not looked up by the worker

Since 2026-06 the worker does **not** consult `instrument-lookup` (it's
WASM, no catalog linked) and no longer relies on the
`PIP_SIZE_<instrument>` secret as the *source of truth*. `tv-arm` reads
`asset.pip_size` from `instrument-lookup` at arm time and bakes it onto the
signed enter intent:

- **Top-level `Intent.pip_size: Option<f64>`** — set for **both** H&S and
  M/W enters. The worker reads this first (`run_enter`, `src/lib.rs`);
  `pip_size_for` (secret → `0.0001` default) is only the fallback for
  intents with no baked value.
- **`MwParams.pip_size`** — M/W also carries pip inside `mw` (its
  mid-correct resolution reads that copy directly). `tv-arm`/the cli set
  both fields to the same number, so don't "fix" one without the other.

`pip_size` is a signed field (whole-body HMAC), so a baked value can't be
tampered. It's also bound into gate scripts as the `pip_size` variable
(`core/src/rules.rs`). The catalog itself: indices report `pip_size = 1.0`
(the sizing point), *not* their sub-point tick — see the
`[[pip_size_vs_tick_size]]` memory and the `instrument-lookup` fix. Don't
reintroduce a worker-side pip lookup or a tick-derived index pip.

## Conventions

- The Rust side follows the parent's CLAUDE.md (2024 edition, no
  `mod.rs`, etc.).
- The Python side is single-file (`scripts/tv_arm_hs.py`, ~800 lines).
  Don't split into a package until a second strategy (M/W) lands.
- tv-mcp is *not* vendored — the script `subprocess`-launches Node
  scripts that import from `~/Downloads/tradingview-mcp-jackson`.
  Hard-coded path, fine for now (one-user tool).
