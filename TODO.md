# TODO — Deprecate TradingView alerts (tv-alerts) + env rename to suffixed `-dev`

Branch: `feat/deprecate-tv-alerts` (worktree). Target env: **dev** (`main`).

## Context

The system has moved from **paid TradingView alerts** (tv-arm builds a signed
5-alert bundle → POSTs to TradingView via tv-mcp → TV fires them → worker
webhook) to a **server-side cron engine** (one signed `TradePlan` registered with
the worker; a `*/15` cron evaluates it against fresh candles and dispatches
fires through the *same* `run_enter`/`run_close`/veto/prep handlers).

The engine path is the keeper. TV alerts are being retired. The worker's inbound
action handlers + gates + `core` parse/verify are **shared** with the engine and
must stay — only the TV-alert *creation* and TV-alert-*specific* worker nuances
are dead.

Env rename (operator's call, 2026-06-22): **everything gets a suffix.**
`main` → dev → worker `trade-control-web-hook-dev`, R2 `trade-control-recording-dev`
(both NEW). The old no-suffix worker `trade-control-web-hook` + R2
`trade-control-recording` are **deprecated, left running** (operator still
journaling last week's trades) and deleted later. `staging` stays `-staging`.
Future `prod` branch → `-prod` suffix; no-suffix retired entirely later.

---

## THIS SESSION

### 1. tv-arm: remove TV-alert creation  — [DONE]
- [x] Delete the `--create-alerts` flag (`args.rs`) + its test assertions.
- [x] Remove pipeline steps 9-10 + `build_all_payloads` + `stamp_payload` +
      the dead imports (`pipeline.rs`).
- [x] Delete modules `create_alerts.rs`, `alert_spec.rs`, `post_outcome.rs`,
      `manifest.rs` (all only fed the dead payload loop) + their `mod` decls.
- [x] Delete `assets/tv_mcp_template.js`.
- [x] Fix doc comments in `register_post.rs` / `pipeline.rs` / `args.rs` that
      framed alerts as live or the engine as "additive/parallel".
- [x] `discover_or_fetch_calendar_bundles` now returns only the in-memory
      `BuiltCalendarBundle`s the engine register path needs.
- [x] build + clippy + fmt clean; 126 tv-arm tests pass; workspace builds.
- [x] README updated. Committed: `aee7651`.

### 2. Repoint `main` to suffixed `-dev` worker + new R2  — [DONE]
- [x] `wrangler.toml`: `name = "trade-control-web-hook-dev"`,
      R2 `bucket_name = "trade-control-recording-dev"`. KV id kept (same dev env,
      just renamed; preserves in-flight plan state).
- [x] `deploy-dev.sh`: `ENV_WEBHOOK = https://trade-control-web-hook-dev.msherborne.workers.dev`;
      stale "web-hook becomes prod" header replaced.
- [x] Created the R2 bucket `trade-control-recording-dev` (empty, safe; no impact
      on the running no-suffix worker).
- [x] Updated env tables: `CLAUDE.md`, `README.md`, `DEPLOYED.md` to the
      "everything suffixed; no-suffix deprecated" model.
- [ ] **Operator action:** deploy from real `main` (`./deploy-dev.sh`) AFTER the
      remaining dev bugs are fixed. Not deployed from this worktree.

### 3. This plan file  — [DONE]

---

## DEFERRED (separate sessions / after dev bugs land)

### D1. Worker: remove TV-alert-specific multi-shot replay special-case
- The engine mints a **fresh intent id per candle**, so the multi-shot
  re-fire-the-same-baked-id machinery is TV-alert-only.
- Candidates: `src/lib.rs` `is_multishot_enter` (~:417) + the multi-shot
  fall-through (~:210-215). Verify the engine dispatch path doesn't lean on it
  before cutting (it synthesises `Verified` and calls `run_enter` directly).
- The `retry_gate` / `EntryAttempt` machinery is **NOT** dead — it's multi-shot
  *re-entry* (place→fill→close→re-place in-window), which the engine still uses.
  Only the *same-id replay* nuance is TV-specific. Tread carefully; this is the
  area the 2026-06 CHF/JPY incident touched.
- Gate behind: engine is the *sole* producer (no TV alerts in flight on any
  live env). Until staging/prod also stop TV alerts, keep it.

### D2. Pine: retire the `alertcondition` surface
- `pine-scripts/candle-signals-v2.pine`: the `alertcondition()` plots exist only
  to fire TV alerts. The engine reads candles directly. The **signal logic** is
  mirrored in `core/src/signals/` (Pine⇄Rust parity memory) — keep that; only the
  `alertcondition`/`alert()` emission is dead.
- `conventions/src/pine.rs` (plot_N index bindings) only matters for TV alert
  wiring — retire alongside.
- Caveat: a chart still needs *a* study for humans to see signals. Decide whether
  to ship a "view-only" Pine (plots, no alertconditions) or leave the study as-is
  and just stop arming alerts. Lean: leave the study, stop arming. Low urgency.

### D3. cli: trim alert-emission portions of the bundle builders
- `cli/src/trade_patterns.rs`, `pause_pattern.rs`, `news_pattern.rs`,
  `calendar_bars.rs` build the signed YAML bundles. The **bundle structs**
  (`BuiltTrade`/`BuiltPause`/`BuiltNews`/`BuiltCalendarBundle`) are consumed by
  `trade_plan_build.rs` for the engine plan — **keep them**.
- Only the parts that exist purely to emit on-disk alert YAML for TV to read can
  go. Much of it is shared (intents are reused by the plan). Audit carefully;
  this is the least clear-cut removal — likely smallest net deletion.

### D4. Remove the deprecated Python script
- `scripts/tv_arm_hs.py` is already a deprecated stub (prints a warning, says use
  Rust tv-arm). Delete the file outright + the README/CLAUDE.md references that
  describe it as the frontend. Provenance `// Port of tv_arm_hs.py::…` comments
  in Rust can stay (history) or be trimmed — cosmetic.

### D5. Env cutover housekeeping (operator-driven, later)
- After journaling last week's trades: delete the no-suffix worker
  `trade-control-web-hook` and its R2 `trade-control-recording`.
- When `prod` branch is cut: `-prod` suffix, deploy-live.sh, prod-pointed
  wrangler.toml. The DEPLOYED.md/CLAUDE.md promotion notes that say
  "web-hook becomes prod" are now superseded by the "everything suffixed" model —
  update them at cutover.
- `deploy-dev.sh` header comment ("next week web-hook becomes PROD…") is stale
  under the new model — update when touched.

---

## Guardrails
- Worktree lives at `trading-libraries/trade-control-web-hook-deprecate-tv-alerts`
  (sibling level) so `../`, `../../`, `../../../trade-calendar-maker` path-deps
  resolve. Do not move it into a nested dir.
- Keep each commit small + green (clippy + fmt + tests). Commit+push per the
  repo's commit-by-default rule; advance the parent gitlink after merge to `main`.
- Do NOT touch staging this session (live demo trading; promotion gate).
