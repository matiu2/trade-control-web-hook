# Replay ↔ worker parity audit

**Goal:** the offline replay (`replay-candles` → `engine::simulate_fill` +
`evaluate_plan`) should make the same trade decisions the live Cloudflare worker
makes, so a replay is a faithful dry-run of production. Every decision the worker
makes that the replay *can't* see is a place the replay silently diverges.

The rule (`[[strategy_changes_in_both_replayer_and_worker]]`): a trade-decision
must live in a crate **both** consumers can depend on — `core` (or `engine`) —
not in the worker crate (`trade-control-web-hook`, a `cdylib` the cli/engine
**cannot** depend on). The good pattern already exists: `pause_gate`,
`retry_gate`, `Breakeven`, `sl_spread_floor`, `entry_level_veto`,
`Resolved::from_intent` all live in `core` and are shared.

This file inventories what the worker's `run_enter` / `run_close` / cron tick
decide that is **still worker-only**, ranked by replay value.

---

## Legend

- **Decision pure?** — is the core logic already a pure fn (just mislocated), or
  is it entangled with KV/broker I/O?
- **Replayable?** — can the offline replay reconstruct the inputs from the
  candle path (incl. bid/ask) + the signed plan, with no live KV/quote?
- **Status** — `shared` (in core, replay uses it), `MOVE` (pure, just needs to
  move to core), `extract` (entangled — needs a pure seam carved out first),
  `n/a` (infra, not a trade decision).

---

## A. `run_enter` gate chain (the entry path) — order as in `src/lib.rs`

| Gate | Where now | Decision pure? | Replayable? | Status | Replay value |
|---|---|---|---|---|---|
| seen-id replay dedup | `core::state` (`is_seen`/`mark_seen`) | yes | yes (MemStateStore) | **shared** | — |
| retry gate (multi-shot) | `core::retry_gate` | yes | yes | **shared** ✅ | — |
| cooldown | `core::state` (`CooldownEntry`) + `cooldown_hours` Rhai in worker | data shared; the *gate eval* is worker-side | partly (Rhai needs the rules engine) | **extract** | medium — a cooldown that would block a re-entry is invisible in replay |
| prep ordering (`requires_preps`) | `engine::evaluate` (retest stamp/gate) | yes | yes | **shared** ✅ | — |
| KV vetos (`is_vetoed`) | `core::state` | yes | yes | **shared** (replay wires MemStateStore) | low — replay rarely seeds vetos |
| at-entry level vetos (Bug #12) | `core::intent::entry_level_veto` | yes | yes | **shared** ✅ (simulate_fill applies) | — |
| `allow_entry` Rhai script | `src/allow_entry_gate.rs` (pure `evaluate`, 415 ln) | **yes** (pure, takes intent+shell+resolved) | yes (Rhai compiles off-wasm) | **MOVE** | **high** — a script `false`/golden-unmet/confirmed-unmet would reject the entry; replay fills it anyway |
| candle-quality (`needs_golden`/`needs_confirmed`) | `src/candle_gate.rs` (pure `evaluate`, 211 ln) | **yes** | yes (latched signal carries flags) | **MOVE** | **high** — but note `evaluate_plan`'s `pine_entry_dispatchable` already pre-flights golden/confirmed for Pine enters; gap is M/W + non-pine |
| market-hours blackout | `src/market_blackout.rs` (pure minute-of-day, 65 ln) + KV windows | **yes** (pure predicate) | yes IF the no-entry windows are resolvable offline (they're daily-cron-written to KV from broker session hours) | **MOVE** (predicate) + need a window source | **high** — a re-entry in a closed session is rejected live, filled in replay |
| spread blackout | `src/spread_blackout.rs` (pure decision + baked baseline, 181 ln) | **yes** | **yes** — `ask_c − bid_c` per candle IS the spread; threshold is the build.rs baseline × 5 | **MOVE** | **high** — the one you asked for; fully reconstructable from candle bid/ask |
| SL ≥ 10×spread floor | `core::intent::sl_spread_floor` | yes | yes (spread from candle) | **shared** ✅ | — |
| RR ≥ 1 floor (`MIN_R_FLOOR`) | `core` resolution | yes | yes | **shared** ✅ | — |
| `recover_entry` (#19-10) | `src/recover_entry.rs` (pure `recover_entry_plan`, 360 ln) | **yes** (pure plan fn) | partly — needs the broker "too close" error, which the sim approximates | **MOVE** (plan) | medium — changes a rejected stop-entry into a recovered one |

## B. `run_close` path

| Gate | Where now | Pure? | Replayable? | Status | Value |
|---|---|---|---|---|---|
| `allow_close` Rhai | `src/allow_close_gate.rs` (pure `evaluate`, 278 ln) | **yes** | yes | **MOVE** | medium — a blocked close would keep a position open in replay vs live |
| reversal-close (PinePattern guard) | `engine::evaluate` + report post-pass | yes | yes | **shared** ✅ | — |
| `veto_on_reversal` write | `core::intent` (REVERSAL_VETO_NAME) | yes | yes | **shared** ✅ | — |

## C. Cron-tick upkeep (per 15-min tick)

| Action | Where | Pure seam? | Replayable? | Status | Value |
|---|---|---|---|---|---|
| break-even watch | `src/cron/breakeven_watch.rs` → `core::Breakeven` | yes | yes | **shared** ✅ (replay shows `be:` line) | — |
| order sweep (expiry + SL-breach) | `src/cron/sweep.rs` (pure `breach_detected`) | **yes** | yes (expiry_bars already honoured in sim; SL-breach computable) | **extract** the breach predicate to core; surface in report | **high** — explains a `NEVER FILLED` (expiry vs SL-breach) |
| spread widen (System 2) | `src/cron/blackout_widen.rs` (pure `widened_stop`) | **yes** | yes (spread from candle) | **MOVE** | medium — a widened stop changes the exit price |
| blackout cancel/restore (System 3) | `src/cron/blackout_cancel.rs` / `_restore.rs` | mixed (KV + broker re-drive) | partly | **extract** | low — complex, needs cancelled-order state |
| spread-recovery watch | `src/cron/blackout_watch.rs` | flag lifecycle | needs spread | **extract** | low |
| market-hours refresh | `src/cron/blackout_hours.rs` | resolves session→windows | the *windows* are the input market-hours gate needs | **extract** the resolver to core | feeds the market-hours gate above |
| session refresh | `src/cron/session_refresh.rs` | — | — | **n/a** (auth infra) | — |
| NY-close-edge apply | `src/cron/blackout_apply.rs` → `core::ny_clock::is_ny_close_edge` | `is_ny_close_edge` already in core | yes (time-based) | partly **shared** | gates the spread-blackout window-open marker |

---

## Priority order (highest replay-fidelity gain first)

1. **spread_blackout → core** (asked for; fully replayable from candle bid/ask).
2. **order sweep breach predicate → core** + report line (explains NEVER FILLED).
3. **market-hours blackout predicate → core** + an offline window source.
4. **allow_entry / candle_gate → core** (Rhai compiles off-wasm; big fidelity win
   for script-gated setups).
5. **allow_close → core**, **spread widen → core**, **recover_entry → core**.

Each is independently shippable. Items 1–3 were the user's pick; 4–5 are the
longer tail. Every move follows the same shape: relocate the pure fn (+ its
tests) to `core`, re-export from the worker so `src/lib.rs` is unchanged at the
call site, then wire the replay (`simulate_fill` / report) to call the now-shared
fn — so worker and replay can't drift.
