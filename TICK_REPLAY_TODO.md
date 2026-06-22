# Engine tick-bundle recording + replay — TODO

Branch `feat/engine-tick-replay` (worktree `../tcwh-engine-replay`), cut from `main`.
Plan: `~/.home-claude/plans/glowing-dancing-sketch.md`. Brief: `ENGINE_REPLAY_RECORDING_PROMPT.md`.
No deploy to staging/prod. Each step its own green commit (test + clippy + fmt + wasm where glue touched).

## (a) core pure types — DONE (commit pending)
- [x] Relocate `FiredIntent` + `PlanEval` defs → `core/src/plan_eval.rs`; re-export from engine lib + evaluate
- [x] Add `Serialize, Deserialize` to: `Candle`, `LatchedSignal`, `FiredIntent`, `PlanEval`
      (dropped `PartialEq` — `Intent` doesn't derive it; compare via serialized JSON instead)
- [x] New `core/src/tick_bundle.rs`: `TickBundle` + `DispatchOutcome` + `KvTickTransition` + `r2_key()`
- [x] serde round-trip unit test (JSON fixture → parse → reserialize → compare serde_json::Value)
- [x] Gate: core 504 + engine 28 tests green; clippy clean; fmt; full workspace builds

## (b) MemStateStore behind test-support feature — DONE (commit pending)
- [x] `mod memstore` #[cfg(test)] → #[cfg(any(test, feature = "test-support"))]
- [x] `test-support = ["dep:serde_json", "chrono/clock"]` (MemStateStore needs both off-test);
      serde_json added as optional dep; `pub use memstore::MemStateStore` gated
- [x] Verified: default tests + --features test-support tests both 504 green; clippy clean both;
      non-test lib build with feature compiles MemStateStore; wasm core build does NOT pull test-support

## (c) record_tick_to_r2 + ScheduleContext threading — SHADOW ticks only — DONE (commit pending)
- [x] src/cron.rs: `_ctx` → `ctx`; pass `&ctx` to run_engine_tick
- [x] thread `ctx: &ScheduleContext` → run_engine_tick → tick_one
- [x] new src/tick_recording.rs: record_tick_to_r2 (mirror record_to_r2, fail-soft, wasm-cfg + native stub)
- [x] tick_one: persist_plan_state captures KvTickTransition; build_tick_bundle helper; emit gated on `plan.shadow`
- [x] R2 key: ticks/<date>/<tick_ts>-<trade_id>.json (TickBundle::r2_key)
- [x] README: "Engine tick-bundles (ticks/ prefix)" subsection
- [x] Gate: workspace tests green; clippy --all-targets -D warnings clean; fmt; worker-build --release OK

## (d) extend to live ticks — DONE (commit pending)
- [x] dispatch_fired returns its outcome string; tick_one collects DispatchOutcome per fire (seq-ordered)
- [x] live + put-failed paths now also build + record a TickBundle (not just shadow)
- [x] README softened: both shadow + live ticks recorded
- [x] Gate: workspace tests green; clippy -D warnings clean; fmt; worker-build --release OK

## (e) native replay CLI — DONE (commit pending)
- [x] cli/src/replay.rs: `trade-control replay <bundle.json>`
- [x] re-run evaluate_plan, diff fired/new_state/done (serde-JSON eq), non-zero exit on mismatch
- [x] cli dep: trade-control-engine added (test-support NOT needed — pure replay; that's for step f)
- [x] 2 unit tests (faithful→MATCH, tampered→MISMATCH) + manual binary smoke (MATCH exit 0, MISMATCH exit 1)
- [x] Gate: workspace tests green (cli 239); clippy -D warnings clean; fmt
- note: vNN tag + CHANGELOG deferred to after step (f) — a–f is one release (next is v33)

## (f) broker-simulator for fill replay — DONE (commit pending)
- [x] engine/src/simulator.rs: pure simulate_fill (resolves via core, fills from recorded candles)
      SimOutcome = NeverFilled / FilledOpen / StoppedOut / TookProfit / Unresolved
- [x] `replay --simulate` resolves each fired enter + walks candle path; prints outcome
- [x] 3 unit tests (TP/SL/never/open + ambiguous→pessimistic-stop) + binary smoke (fill→TP)
- [SCOPED OUT] full Broker-trait impl + dispatch_outcomes replay through run_enter/run_close:
      blocked by worker::Response panicking off-wasm (handlers are pub(crate) in the cdylib).
      Documented as Follow-up in CHANGELOG v33. User-approved: build simulator, defer dispatch replay.
- [x] Gate: workspace tests green (engine 31); clippy -D warnings clean; fmt; wasm + worker-build OK

## Cross-cutting — DONE
- [x] CHANGELOG v33 + README replay/--simulate + ticks/ prefix notes
- [x] tag v33 applied + pushed
- [x] memory: new `engine_tick_replay_landed` + held memory cross-linked + MEMORY.md index updated

## ALL STEPS (a)–(f) DONE. Branch feat/engine-tick-replay @ v33, pushed. NOT merged to main, no deploy.
