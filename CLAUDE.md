# trade-control-web-hook — orientation for Claude

Two layers live in this repo:

1. **Rust worker + CLI** (`src/`, `core/`, `cli/`, `broker-oanda/`) — the
   Cloudflare Worker that receives signed TradingView alerts and the
   `trade-control` CLI that signs them. The README covers the wire format,
   actions (`enter`/`prep`/`veto`/...), and CLI subcommands in depth.
2. **Chart-driven Python tool** (`scripts/tv_arm_hs.py`) — reads a
   TradingView head-and-shoulders chart via tv-mcp and produces the full
   alert bundle for one setup by shelling out to `trade-control build-trade
   --from-file`. The README has a section on it; this file is the
   "stuff a future Claude will get bitten by" deeper note.

Read the README first for the user-facing story. This file is for hazards.

## Branches are environments — know which one you're on

Each git **branch is a deploy environment**, and each carries its **own
`wrangler.toml`** (own worker name, KV namespace, R2 bucket). A plain
`wrangler deploy` on a branch targets that environment — so the branch you
have checked out decides which live worker you'd affect. Check it before
deploying anything.

| branch | environment | worker | CLIs | who uses it |
|---|---|---|---|---|
| `main` | **dev** | `trade-control-web-hook-dev` | `*-dev` | coding / development |
| `staging` | **staging (demo)** | `trade-control-web-hook-staging` | `*-staging` | the week's live demo trading |
| `prod` | **prod (real money)** | `trade-control-web-hook-prod` | `*-prod` | **not stood up yet** — first promotion target |

**Every environment now carries a suffix** (`-dev` / `-staging` / `-prod`).
The old **no-suffix** worker `trade-control-web-hook` + its R2 bucket
`trade-control-recording` are **deprecated**: left running only while last
week's demo trades are journaled, then deleted. Don't deploy to them.

Current working split (2026-06): **trading runs on `staging`** (demo
account, real-time), **coding happens on `main`**. So treat the `staging`
worker as live — don't redeploy it casually mid-week, because a week of
unchanged + profitable running is the promotion gate. Develop on `main`,
let it bake on `staging`.

**Deploy** with the per-environment scripts, never bare `wrangler deploy`
(the scripts also rebuild + install the matching suffixed CLIs and bake the
right webhook URL into them):

```sh
git checkout main    && ./deploy-dev.sh       # dev
git checkout staging && ./deploy-staging.sh   # staging
# ./deploy-live.sh is added at the first prod promotion.
```

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

`prod` doesn't exist yet. The plan: when `staging` has run a full week
unchanged + profitable, it gets merged into a new `prod` branch with a
prod-pointed `wrangler.toml`, and a fresh `staging` is cut from `main`.
Under the **everything-suffixed** model, prod is its own worker
`trade-control-web-hook-prod` (R2 `trade-control-recording-prod`) — a clean
new env, *not* a rename of an existing worker. `deploy-live.sh` (added at
that point) points at `-prod`; `main`/`-dev` and `staging`/`-staging` keep
their own workers. The legacy no-suffix worker is retired separately (after
the journaling window) and is **not** repurposed as prod. Keep each branch's
`wrangler.toml` divergent and pointed at its own suffixed worker.

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
the visibility gain — and gate rejections are already visible in
Cloudflare Real-time Logs via the `console_log!` line in
`log_skip`.

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

### Intrabar crosses fire on the wick (from the open side), not the close

**Updated 2026-07-01 (`engine` commit `f231629`).** This reverts the earlier
"too-high is close-confirmed" rule. The engine's `level_crossed`
(`engine/src/evaluate.rs`) **`BarEvent::Intrabar`** arm now reads the wick on
the cross side, **close-agnostic**, discriminated by which side the bar *came
from* (its `open`):

- `Up`     ⇒ `open <= level && high >= level`  (came from below, reached above)
- `Down`   ⇒ `open >= level && low  <= level`  (came from above, reached below)
- `Either` ⇒ any straddle (`low <= level <= high`) — unchanged

The intuition (operator's framing): on a tick timeline a bar that opened on one
side of the level and traded through to the other **did** cross, even if it
closed back on the original side. The old rule required a confirming **close**
(`Up ⇒ c >= level`), which silently dropped a retest tap-and-bounce — the bug
this fixed (AUD/JPY iH&S long 2026-06-29: a 6pm bar opened above the descending
neckline, wicked below, closed back above → the retest didn't stamp for ~6h).
See `[[intrabar_cross_reads_wick_not_close]]`.

**`BarEvent::OnClose` is unchanged** — `03-prep-break-and-close` still requires a
genuine close through the line (open one side, **close** the other). Only the
intrabar arm moved.

Consumers of the intrabar arm, all now wick-from-the-open-side (for a **short**;
mirror for long):

- **`too-high` = invalidation** (drawing-bound horizontal at the shoulder cap).
  `HorizontalCross { dir: Up, bar: Intrabar }` from
  `tv-arm/src/trade_plan_build.rs::invalidation_or_pcl_trigger`. A bar that
  opened below `too-high` and whose **high reaches above** it now fires — a
  spike-and-recover that crossed up from below invalidates intrabar. A bar that
  merely *closes* above without crossing up from below does **not**. (The old
  close-confirm NZD/CHF example no longer applies as written.)
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

**Follow-up (not built yet):** a tunable cross-depth buffer so a one-tick graze
doesn't trigger — must cross by N ticks or ~0.1% of the line price. Designing
next.

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
