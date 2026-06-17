# Changelog

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
